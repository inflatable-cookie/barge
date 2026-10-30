//! Project scratch: allocation, conclusion and the expiry sweep with its
//! recorded grants (stage 1 design, "Project scratch areas").
//!
//! Slice S4. Three rules shape the module.
//!
//! - **Only a grant authorizes.** A native root is its own grant (Barge created
//!   it and recorded a conclusion); a foreign marker family is deleted only
//!   under that family's recorded delegation, and an unmarked unit only under a
//!   human's recorded legacy claim for its scope. No name, age or marker's
//!   absence is ever inferred into authority, and an empty `scratch_grants`
//!   means keep and report.
//! - **Preview then real.** The sweep computes one stable plan and its SHA-256.
//!   A real run requires exactly that digest, then re-verifies every predicate
//!   (grant, marker, identity, holds, age, symlink-freedom) immediately before
//!   each effect, and deletes only through S5's custody and delete path.
//! - **Nothing unresolvable is acted on.** A foreign marker is parsed strictly
//!   against its family's pinned field set; an unknown version, an extra field,
//!   a wrong type, a foreign owner or an unreadable file keeps the unit. A
//!   symlink is never followed, a unit name is never allowed to escape the folder
//!   it names, and a bound that is reached leaves the rest for a later run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::area::{
    self, AbsentPolicy, AreaDeclaration, ConclusionClosure, ConclusionOutcome, HoldRequest,
    HoldScope, HoldState, KeptReason, MarkerExpectation, MarkerState, NestedScope, PrunePolicy,
    ReserveParticipation, RootConclusion, RootKind, RootSpec,
};
use crate::contract::{RowSet, LISTING_NAME_ROWS, MARKER_CAP_BYTES, ROOT_ROWS};
use crate::custody::PendingIntent;
use crate::delete::{self, DeleteBounds, PendingRow};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    AbsenceVolume, DeviceId, DirHandle, DirOps, EntryFacts, EntryKind, MountRoot, ProcessObserver,
    VolumeFacts, VolumeUuid,
};
use crate::registry::{
    self, ClaimScope, ClaimUnit, MarkerFamily, ScratchClaim, ScratchDelegation, ScratchGrants,
    VolumeRecord,
};
use crate::state::{self, StatePaths};
use crate::unit::{
    DeletionUnit, MarkerFamily as UnitMarkerFamily, UnitGrant, UnitKind, UnitMarker,
};

#[derive(Debug, Clone)]
struct ObservedRoot {
    device: DeviceId,
    inode: u64,
    marker_digest: String,
}

/// The namespace every scratch root's marker records. Barge's own folder is not
/// a consumer area, so it uses its own fixed identity.
pub const SCRATCH_NAMESPACE: &str = "barge";

/// The area name every scratch root's marker records.
pub const SCRATCH_AREA: &str = "scratch";

/// Barge's scratch folder when a volume overrides none.
pub const DEFAULT_SCRATCH_FOLDER: &str = "scratch";

/// The purpose an allocation records on its owner hold. Conclusion requires it,
/// so the recorded owner is a fact in `barge.hold/1` and not a claim the CLI
/// makes about itself.
pub const OWNER_HOLD_PURPOSE: &str = "barge scratch alloc owner";

/// How long a positively concluded native root is kept before expiry.
pub const NATIVE_AGE_SECONDS: u64 = 7 * 24 * 60 * 60;

/// How long a delegated Oxide root's positive conclusion is kept.
pub const OXIDE_AGE_SECONDS: u64 = 7 * 24 * 60 * 60;

/// How long a concluded Effigy run that asked to be kept is retained.
pub const EFFIGY_KEEP_AGE_SECONDS: u64 = 24 * 60 * 60;

/// How long an Effigy run whose liveness cannot be decided outlives.
pub const EFFIGY_AGE_SECONDS: u64 = 7 * 24 * 60 * 60;

/// How many entries one sweep examines (design, "Bounded resources").
pub const SWEEP_EXAMINE: usize = 4096;

/// How long one sweep may spend examining entries.
pub const SWEEP_TIMEOUT: Duration = Duration::from_secs(1);

/// The Oxide scratch marker, written and concluded by Oxide's supervisor only.
pub const OXIDE_MARKER_FILE: &str = ".oxide-scratch.json";

/// The Effigy scratch marker, written and concluded by Effigy only.
pub const EFFIGY_MARKER_FILE: &str = ".effigy-scratch.json";

/// Oxide's validation temp folder at the scratch root: never a Barge root, never
/// covered by a claim, always skipped.
pub const OXIDE_VALIDATION_DIR: &str = "oxide-validation";

/// The project folder the host reaper leaves to Oxide's own supervisor.
pub const RESERVED_PROJECT: &str = "oxide";

/// Every marker file whose presence makes a unit somebody's marked root.
pub const MARKER_FILES: [&str; 3] = [
    area::ROOT_MARKER_FILE,
    OXIDE_MARKER_FILE,
    EFFIGY_MARKER_FILE,
];

/// The longest unit name a plan row or state key may carry. A longer name is
/// unknown and is reported, never acted on.
const MAX_UNIT_NAME_BYTES: usize = 255;

/// The longest legacy claim's idle age: a century, so a value the platform
/// cannot represent is refused rather than truncated.
const MAX_CLAIM_IDLE_SECONDS: u64 = 100 * 365 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// The scratch folder and its declaration
// ---------------------------------------------------------------------------

/// The scratch folder one volume resolves: its override, else `scratch/`.
pub fn scratch_folder(record: &VolumeRecord) -> String {
    record
        .folders
        .scratch
        .clone()
        .unwrap_or_else(|| DEFAULT_SCRATCH_FOLDER.to_owned())
}

/// The declaration a scratch folder is resolved with.
///
/// Scratch is Barge's own folder, not a consumer area, so this declaration is
/// built here rather than stored: its namespace and area are fixed, its folder
/// is the one this call acts in (the scratch folder, or one project folder
/// below it), and its policies are the design's constants. O-003 keeps the
/// caller's declaration authoritative, so nothing stored can widen it.
pub fn scratch_declaration(record: &VolumeRecord, folder: &str) -> AreaDeclaration {
    AreaDeclaration {
        namespace: SCRATCH_NAMESPACE.to_owned(),
        area: SCRATCH_AREA.to_owned(),
        folder: folder.to_owned(),
        overrides: BTreeMap::new(),
        absent: AbsentPolicy::Fail {
            attach: record.volume_uuid.to_string(),
        },
        prune: PrunePolicy::OldestIdleAfter {
            age_seconds: NATIVE_AGE_SECONDS,
        },
        reserve: ReserveParticipation::Counts,
    }
}

/// The folder string a unit's parent folder has, given the scratch folder.
fn project_folder(scratch: &str, project: &str) -> String {
    format!("{scratch}/{project}")
}

/// Whether a name is Effigy's derived run-folder shape, which Barge never
/// descends into as a project folder.
fn is_run_name(name: &str) -> bool {
    name.len() == 8
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

/// What one caller asks `barge scratch alloc` for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocRequest {
    /// The registered volume to allocate on.
    pub volume_uuid: VolumeUuid,
    /// The project folder below the scratch folder.
    pub project: String,
    /// The consumer's own id, recorded as the root's owner hold.
    pub consumer: String,
    /// The consumer process that will use the root.
    pub pid: u32,
}

/// The `barge.scratch.alloc/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocReport {
    /// The registered volume the root was created on.
    pub volume_uuid: String,
    /// The volume's proved mount point.
    pub mount_point: PathBuf,
    /// The scratch folder the volume resolves.
    pub scratch_folder: String,
    /// The project folder the root lives in.
    pub folder: String,
    /// The project the root belongs to.
    pub project: String,
    /// The consumer recorded as the root's owner.
    pub consumer: String,
    /// The root that was created, with its identity and marker digest.
    pub root: area::OwnedRoot,
}

/// Allocate one project scratch root on a registered volume.
///
/// The volume is verified first, the project folder is created as a plain
/// directory of this user when it is absent, and one root is created exclusively
/// with a fresh 128-bit id, `0700` and its marker before any payload, so two
/// concurrent allocators can never receive the same root. The consumer's owner
/// hold is recorded immediately after, because conclusion and expiry both act on
/// a hold that is real.
pub fn alloc(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &AllocRequest,
) -> Result<AllocReport> {
    area::validate_name("project", &request.project)?;
    area::validate_name("consumer", &request.consumer)?;
    let record = area::registered_volume(state_paths, &request.volume_uuid)?;
    record.declared_durability()?;
    let scratch = scratch_folder(&record);
    area::folder_components("folders.scratch", &scratch)?;
    let folder = project_folder(&scratch, &request.project);
    let declaration = scratch_declaration(&record, &folder);
    let (volume, root) = area::create_root_on_volume(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        &request.volume_uuid,
        &declaration,
        None,
        RootSpec {
            kind: RootKind::ScratchRun,
        },
    )?;
    let hold = area::hold(
        state_paths,
        observer,
        &HoldRequest {
            root_id: root.root_id.clone(),
            holder: request.consumer.clone(),
            pid: request.pid,
            scope: HoldScope::Write,
            purpose: OWNER_HOLD_PURPOSE.to_owned(),
        },
    )
    .map_err(|error| {
        error
            .with_detail("root_id", root.root_id.clone().into())
            .with_detail("path", root.path.display().to_string().into())
    })?;
    let _ = hold;
    Ok(AllocReport {
        volume_uuid: volume.record.volume_uuid.as_str().to_owned(),
        mount_point: volume.resolved.mount_point.clone(),
        scratch_folder: scratch,
        folder,
        project: request.project.clone(),
        consumer: request.consumer.clone(),
        root,
    })
}

// ---------------------------------------------------------------------------
// Conclusion
// ---------------------------------------------------------------------------

/// What one caller asks `barge scratch conclude` for. The five conclusion facts
/// are the design's own shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcludeRequest<'a> {
    /// The registered volume the root lives on.
    pub volume_uuid: &'a VolumeUuid,
    /// The root to conclude.
    pub root_id: &'a str,
    /// The consumer recorded as the owner by allocation.
    pub holder: &'a str,
    /// The project folder, when the caller knows it. Absent means the bounded
    /// search below finds the root.
    pub project: Option<&'a str>,
    /// How the run closed.
    pub outcome: ConclusionOutcome,
    /// Whether closure is positive.
    pub closure: ConclusionClosure,
    /// Why the root was kept.
    pub kept_reason: KeptReason,
    /// What the conclusion covers.
    pub nested_scope: NestedScope,
}

/// The `barge.scratch.conclude/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcludeReport {
    /// The registered volume the root lives on.
    pub volume_uuid: String,
    /// The root that was concluded.
    pub root_id: String,
    /// The project folder the root was found in.
    pub folder: String,
    /// The root's absolute path.
    pub path: PathBuf,
    /// The conclusion as written.
    pub conclusion: RootConclusion,
    /// The SHA-256 of the marker bytes after the conclusion was written.
    pub marker_digest: String,
}

/// Where a root was found.
struct FoundRoot {
    folder: String,
    root: DirHandle,
    path: PathBuf,
    marker: area::RootMarker,
    marker_digest: String,
    mount_point: PathBuf,
}

/// What a bounded search for one root found.
enum Found {
    One(Box<FoundRoot>),
    None,
    Ambiguous(usize),
}

/// Read one root in one known folder, or report it absent there.
#[allow(clippy::too_many_arguments)]
fn find_in_folder(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    folder: &str,
    root_id: &str,
    now_ms: u64,
) -> Result<Option<FoundRoot>> {
    let Some((path, handle)) = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &record.root_dir,
        folder,
    )?
    else {
        return Ok(None);
    };
    match dirs.child_facts(&handle, root_id)? {
        Some(facts) if facts.kind == EntryKind::Directory => {}
        _ => return Ok(None),
    }
    let root = dirs.open_child_dir(&handle, root_id)?;
    let root_path = path.join(root_id);
    let expectation = MarkerExpectation {
        volume_uuid: &record.volume_uuid,
        resolved_path: &root_path,
        mount_point: record.mount_hint.as_deref(),
        effective_uid: observer.effective_uid()?,
        now_ms,
    };
    let (marker, digest) = area::read_marker_with_digest(dirs, &root, &expectation);
    let marker = match marker {
        MarkerState::Owned(marker) if marker.kind == RootKind::ScratchRun => *marker,
        other => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "root `{root_id}` in `{folder}` is not an owned scratch run: {}",
                    describe_marker(&other)
                ),
            )
            .with_detail("root_id", root_id.to_owned().into())
            .with_detail("folder", folder.to_owned().into()))
        }
    };
    let Some(marker_digest) = digest else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the root's marker digest could not be read".to_owned(),
        ));
    };
    if let Some(effective) = &marker.area.effective {
        if effective.folder != folder {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "root `{root_id}` records the folder `{}`, not `{folder}`",
                    effective.folder
                ),
            ));
        }
    }
    Ok(Some(FoundRoot {
        mount_point: record
            .mount_hint
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::UnknownState, "verified mount hint missing"))?,
        folder: folder.to_owned(),
        root,
        path: root_path,
        marker,
        marker_digest,
    }))
}

/// Find one root by name across the scratch root's project folders, bounded.
///
/// The search is read-only and never follows a link: only a real project folder
/// that contains a real child of that name can hold the root, and a folder whose
/// candidate is not an owned scratch run is a refusal rather than a skip.
#[allow(clippy::too_many_arguments)]
fn find_by_search(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    scratch: &str,
    root_id: &str,
    now_ms: u64,
) -> Result<Found> {
    let Some((_path, handle)) = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &record.root_dir,
        scratch,
    )?
    else {
        return Ok(Found::None);
    };
    let listing = dirs.list_children(&handle, LISTING_NAME_ROWS)?;
    let mut names: Vec<String> = listing
        .entries
        .iter()
        .filter(|entry| entry.facts.kind == EntryKind::Directory)
        .map(|entry| entry.name.clone())
        .collect();
    names.sort();
    let mut found: Option<FoundRoot> = None;
    let mut count = 0usize;
    for project in names.iter().take(SWEEP_EXAMINE) {
        if project == OXIDE_VALIDATION_DIR || project == RESERVED_PROJECT {
            continue;
        }
        let candidate = project_folder(scratch, project);
        if let Some(root) = find_in_folder(
            dirs, facts, mounts, observer, record, &candidate, root_id, now_ms,
        )? {
            count += 1;
            if found.is_none() {
                found = Some(root);
            }
        }
    }
    match found {
        None => Ok(Found::None),
        Some(root) if count == 1 => Ok(Found::One(Box::new(root))),
        Some(_) => Ok(Found::Ambiguous(count)),
    }
}

/// Record one scratch run's conclusion, once.
///
/// The caller must be the recorded owner (allocation's owner hold, live), the
/// root's marker must still be the bytes and inode it was read at, and the claim
/// of positive closure must be supported by the evidence Barge can establish: no
/// other live or undecidable hold on the root and no pending intent naming it. A
/// root that cannot claim positive closure records `closure: none`, which is an
/// honest non-conclusion rather than a refusal.
pub fn conclude(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &ConcludeRequest<'_>,
) -> Result<ConcludeReport> {
    state::validate_identifier("root_id", request.root_id)?;
    area::validate_name("holder", request.holder)?;
    if let Some(project) = request.project {
        area::validate_name("project", project)?;
    }
    // A positive closure is the claim that the run closed; recording it beside a
    // retained, unclosed or failed outcome would authorize an expiry the owner's
    // own facts contradict. Refuse the combination at the write, so no invalid
    // conclusion fact ever lands in a marker.
    if request.closure == ConclusionClosure::Positive
        && request.outcome != ConclusionOutcome::Closed
    {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "a positive closure is only recorded for a closed run, not `{}`",
                outcome_name(request.outcome)
            ),
        )
        .with_detail("outcome", outcome_name(request.outcome).into())
        .with_detail("closure", "positive".into()));
    }
    let record = area::registered_volume(state_paths, request.volume_uuid)?;
    record.declared_durability()?;
    let scratch = scratch_folder(&record);
    let now_ms = state::now_ms();

    state_paths.initialize()?;
    let mut locks = state::LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(request.volume_uuid)?;
    let _root_lock = locks.lock_root(request.root_id)?;

    let found = match request.project {
        Some(project) => {
            let root = find_in_folder(
                dirs,
                facts,
                mounts,
                observer,
                &record,
                &project_folder(&scratch, project),
                request.root_id,
                now_ms,
            )?;
            root.map_or(Found::None, |root| Found::One(Box::new(root)))
        }
        None => find_by_search(
            dirs,
            facts,
            mounts,
            observer,
            &record,
            &scratch,
            request.root_id,
            now_ms,
        )?,
    };
    let found = match found {
        Found::One(found) => *found,
        Found::None => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "no scratch root `{}` was found in the scratch folder `{scratch}`",
                    request.root_id
                ),
            )
            .with_detail("root_id", request.root_id.to_owned().into())
            .with_detail("folder", scratch.into()))
        }
        Found::Ambiguous(count) => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "root `{}` was found in {count} project folders; pass --project to name one",
                    request.root_id
                ),
            )
            .with_detail("root_id", request.root_id.to_owned().into()))
        }
    };
    if found.marker.conclusion.is_some() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "root `{}` already carries a conclusion, and a conclusion is terminal",
                request.root_id
            ),
        )
        .with_detail("root_id", request.root_id.to_owned().into()));
    }

    // The recorded owner: allocation's owner hold, still live.
    let holds = area::read_holds(state_paths, observer, request.root_id)?;
    match holds
        .holds
        .iter()
        .find(|(hold, _, _)| hold.holder == request.holder)
    {
        Some((hold, HoldState::Live, _)) if hold.purpose == OWNER_HOLD_PURPOSE => {}
        Some((hold, state_name, _)) => {
            return Err(Error::new(
                ErrorKind::CustodyNotProven,
                format!(
                    "the hold `{}` on root `{}` is {} and cannot conclude it",
                    hold.holder,
                    request.root_id,
                    match state_name {
                        HoldState::Live => "not the allocation's owner hold",
                        HoldState::Stale => "stale",
                        HoldState::Unknown => "undecidable",
                    }
                ),
            )
            .with_detail("holder", request.holder.to_owned().into()))
        }
        None => {
            return Err(Error::new(
                ErrorKind::CustodyNotProven,
                format!(
                    "`{}` holds no live hold on root `{}`, so it is not the recorded owner",
                    request.holder, request.root_id
                ),
            )
            .with_detail("holder", request.holder.to_owned().into()))
        }
    }
    if request.closure == ConclusionClosure::Positive {
        // Positive closure is the owner's claim, and Barge records it only on
        // the evidence it can establish: no other holder and no pending deletion.
        for (hold, state_name, _) in &holds.holds {
            if hold.holder == request.holder {
                continue;
            }
            if matches!(state_name, HoldState::Live | HoldState::Unknown) {
                return Err(Error::new(
                    ErrorKind::CustodyNotProven,
                    format!(
                        "root `{}` carries another live or undecidable hold (`{}`), so positive closure is not supported",
                        request.root_id, hold.holder
                    ),
                )
                .with_detail("holder", hold.holder.clone().into()));
            }
        }
        let intents = state::read_intents(state_paths, crate::contract::INTENT_ROWS)?;
        if let Some(intent) = PendingIntent::pending(&intents.records)
            .into_iter()
            .find(|intent| intent.name == request.root_id)
        {
            return Err(Error::new(
                ErrorKind::CustodyNotProven,
                format!(
                    "root `{}` has the pending intent {}, so positive closure is not supported",
                    request.root_id, intent.intent_id
                ),
            ));
        }
    }

    let conclusion = RootConclusion {
        concluded_ms: now_ms,
        outcome: request.outcome,
        closure: request.closure,
        kept_reason: request.kept_reason,
        nested_scope: request.nested_scope,
    };
    let marker_digest = write_conclusion(dirs, observer, request.volume_uuid, &found, &conclusion)?;
    Ok(ConcludeReport {
        volume_uuid: request.volume_uuid.as_str().to_owned(),
        root_id: request.root_id.to_owned(),
        folder: found.folder,
        path: found.path,
        conclusion,
        marker_digest,
    })
}

/// One concluded marker, written atomically and read back.
fn write_conclusion(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    found: &FoundRoot,
    conclusion: &RootConclusion,
) -> Result<String> {
    // Re-read the marker immediately before the write and compare the identity
    // the read returned: a conclusion never lands on a marker another process
    // replaced.
    let expectation = MarkerExpectation {
        volume_uuid,
        resolved_path: &found.path,
        mount_point: Some(&found.mount_point),
        effective_uid: observer.effective_uid()?,
        now_ms: state::now_ms(),
    };
    let (current, digest) = area::read_marker_with_digest(dirs, &found.root, &expectation);
    let current = match current {
        MarkerState::Owned(marker) if marker.root_id == found.marker.root_id => marker,
        other => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the root's marker changed before the conclusion could be written: {}",
                    describe_marker(&other)
                ),
            )
            .with_detail("root_id", found.marker.root_id.clone().into()))
        }
    };
    if digest.as_deref() != Some(found.marker_digest.as_str()) {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the root's marker bytes changed before the conclusion could be written".to_owned(),
        )
        .with_detail("root_id", found.marker.root_id.clone().into()));
    }
    let mut next = (*current).clone();
    next.conclusion = Some(*conclusion);
    next.generation = next.generation.saturating_add(1);
    let mut bytes = serde_json::to_vec_pretty(&next).map_err(|error| {
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
                "the concluded marker would be {} bytes, over the {MARKER_CAP_BYTES}-byte cap",
                bytes.len()
            ),
        ));
    }
    let written = sha256_hex(&bytes);
    dirs.publish_child_file(
        &found.root,
        area::ROOT_MARKER_FILE,
        &bytes,
        area::RECORD_MODE,
    )?;
    let (back, digest) = area::read_marker_with_digest(dirs, &found.root, &expectation);
    match back {
        MarkerState::Owned(marker) if marker.conclusion == Some(*conclusion) => {}
        other => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the marker just written did not validate on re-read: {}",
                    describe_marker(&other)
                ),
            )
            .with_detail("root_id", found.marker.root_id.clone().into()))
        }
    }
    if digest.as_deref() != Some(written.as_str()) {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the marker read back is not the bytes that were written".to_owned(),
        ));
    }
    Ok(written)
}

fn describe_marker(state: &MarkerState) -> String {
    match state {
        MarkerState::Absent => "it is absent".to_owned(),
        MarkerState::Owned(_) => "it is owned".to_owned(),
        MarkerState::Unknown { reason } => reason.clone(),
    }
}

// ---------------------------------------------------------------------------
// Foreign marker families
// ---------------------------------------------------------------------------

/// What reading one foreign marker file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerReading<T> {
    /// No marker file at all.
    Absent,
    /// A marker that validated against its family's pinned field set.
    Present {
        /// The parsed marker.
        marker: T,
        /// The SHA-256 of the exact bytes read.
        digest: String,
    },
    /// A marker that is there but is not usable: not this user's regular file,
    /// oversized, unparsable, a wrong type, an unknown version or an extra field.
    Unknown {
        /// Why the marker is not usable, for a human.
        reason: String,
    },
}

/// Oxide's `.oxide-scratch.json`, exactly the field set its supervisor writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OxideScratchMarker {
    /// Always 1.
    pub version: u64,
    /// The run's 32-hex id.
    pub run_id: String,
    /// The root's absolute path, which must be the path Barge walked.
    pub root: String,
    /// The owning uid, which must be this account's.
    pub owner_uid: u32,
    /// The root's device.
    pub device: u64,
    /// The root's inode.
    pub inode: u64,
    /// When the root was created, in Unix seconds.
    pub created_unix: f64,
    /// When it was concluded, in Unix seconds, or null while running.
    pub concluded_unix: Option<f64>,
    /// `running`, `closed`, `unclosed`, `failed` or `retained`.
    pub outcome: String,
    /// `positive` or `none`.
    pub closure: String,
    /// `none`, `evidence`, `unclosed`, `exception`, `nested-unclosed`,
    /// `qualify-base` or `qualify-failed`.
    pub kept_reason: String,
    /// `exclusive` or `hosting`.
    pub nested_scope: String,
}

/// Effigy's `.effigy-scratch.json`, exactly the field set its `Marker` writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffigyScratchMarker {
    /// The run's id.
    pub run: String,
    /// The task the run served; Effigy defaults it to empty.
    #[serde(default)]
    pub task: String,
    /// The checkout that started the run; Effigy defaults it to empty.
    #[serde(default)]
    pub checkout: String,
    /// The creating process; Effigy defaults it to zero.
    #[serde(default)]
    pub pid: u32,
    /// The creating process's start identity, as Effigy records it as text.
    #[serde(default)]
    pub pid_start: String,
    /// When the directory was created.
    pub created_ms: i64,
    /// `true` once the owning run recorded that the directory is retained.
    #[serde(default)]
    pub keep: Option<bool>,
    /// When the owning run recorded its conclusion.
    #[serde(default)]
    pub concluded_ms: Option<i64>,
}

/// Read one foreign family's marker strictly.
pub fn read_family_marker<T: DeserializeOwned>(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    unit: &DirHandle,
    file: &str,
    private: bool,
) -> MarkerReading<T> {
    let facts = match dirs.child_facts(unit, file) {
        Ok(Some(facts)) => facts,
        Ok(None) => return MarkerReading::Absent,
        Err(error) => {
            return MarkerReading::Unknown {
                reason: error.to_string(),
            }
        }
    };
    if facts.kind != EntryKind::File {
        return MarkerReading::Unknown {
            reason: format!("`{file}` is a {}, not a regular file", facts.kind.as_str()),
        };
    }
    let uid = match observer.effective_uid() {
        Ok(uid) => uid,
        Err(error) => {
            return MarkerReading::Unknown {
                reason: format!("the account uid is unreadable: {error}"),
            }
        }
    };
    if facts.uid != uid {
        return MarkerReading::Unknown {
            reason: format!("`{file}` is owned by uid {}, not this account's", facts.uid),
        };
    }
    if private && !facts.is_private() {
        return MarkerReading::Unknown {
            reason: format!(
                "`{file}` has mode {:o}, which is not private",
                facts.mode & 0o777
            ),
        };
    }
    if facts.size > MARKER_CAP_BYTES as u64 {
        return MarkerReading::Unknown {
            reason: format!("`{file}` is {} bytes, over the marker cap", facts.size),
        };
    }
    let bytes = match dirs.read_child_file(unit, file, MARKER_CAP_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return MarkerReading::Unknown {
                reason: format!("`{file}` disappeared while it was read"),
            }
        }
        Err(error) => {
            return MarkerReading::Unknown {
                reason: format!("`{file}` is unreadable: {error}"),
            }
        }
    };
    let digest = sha256_hex(&bytes);
    match serde_json::from_slice::<T>(&bytes) {
        Ok(marker) => MarkerReading::Present { marker, digest },
        Err(error) => MarkerReading::Unknown {
            reason: format!("`{file}` is not its family's document: {error}"),
        },
    }
}

/// Read one family marker file's digest, for a custody identity proof.
///
/// This is the same file checks a grant decision makes: a regular file of this
/// user on the volume's device, at most the marker cap, readable. It returns the
/// bytes' digest and nothing else, because the caller compares it with the
/// observation it recorded.
pub fn read_family_marker_digest(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    unit: &DirHandle,
    file: &str,
    device: DeviceId,
) -> Result<String> {
    let facts = dirs.child_facts(unit, file)?.ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            format!("`{file}` is not there, so the unit is not the one observed"),
        )
    })?;
    if facts.kind != EntryKind::File {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{file}` is a {}, not a regular file", facts.kind.as_str()),
        ));
    }
    if facts.uid != observer.effective_uid()? {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{file}` is another user's"),
        ));
    }
    if facts.device != device {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "`{file}` is on device {}, not the unit's {device}",
                facts.device
            ),
        ));
    }
    let bytes = dirs
        .read_child_file(unit, file, MARKER_CAP_BYTES)?
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                format!("`{file}` disappeared while it was read"),
            )
        })?;
    Ok(sha256_hex(&bytes))
}

/// The provenance digest a claimed unmarked unit is recorded by.
///
/// An unmarked unit has no marker bytes, so its identity proof is the volume,
/// its name and its device and inode together. The digest is recomputed from the
/// intent's own fields on replay, so an unmarked deletion is resumed only from
/// the identity it was recorded with.
pub fn unmarked_provenance_digest_parts(
    volume_uuid: &VolumeUuid,
    name: &str,
    device: u64,
    inode: u64,
) -> String {
    sha256_hex(
        format!("barge.unmarked-unit/1\n{volume_uuid}\n{name}\n{device}\n{inode}\n").as_bytes(),
    )
}

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

/// The `barge.scratch.grants/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsReport {
    /// The registered volume the grants belong to.
    pub volume_uuid: String,
    /// The recorded authority: empty means keep and report.
    pub grants: ScratchGrants,
    /// Whether this call changed anything.
    pub changed: bool,
}

/// Read one volume's recorded scratch grants. Nothing is written.
pub fn grants(state_paths: &StatePaths, volume_uuid: &VolumeUuid) -> Result<GrantsReport> {
    let record = area::registered_volume(state_paths, volume_uuid)?;
    Ok(GrantsReport {
        volume_uuid: record.volume_uuid.as_str().to_owned(),
        grants: record.scratch_grants,
        changed: false,
    })
}

/// Declare, or withdraw, one family's delegation on one volume.
///
/// A delegation is the family owner's declaration that Barge may expire that
/// family's roots on this volume under that family's own rule. Barge records who
/// declared it and cannot authenticate that claim: the review is the operator's,
/// and an empty or withdrawn delegation means keep and report.
pub fn declare_delegation(
    state_paths: &StatePaths,
    volume_uuid: &VolumeUuid,
    family: MarkerFamily,
    by: &str,
    now_ms: u64,
    revoke: bool,
) -> Result<GrantsReport> {
    if by.trim().is_empty() {
        return Err(Error::new(
            ErrorKind::Usage,
            "a delegation must name who declared it".to_owned(),
        ));
    }
    if by.len() > 256 {
        return Err(Error::new(
            ErrorKind::Usage,
            "the declaring name is bounded to 256 bytes".to_owned(),
        ));
    }
    update_grants(state_paths, volume_uuid, |grants| {
        let before = grants.delegations.len();
        grants.delegations.retain(|record| record.family != family);
        let removed = grants.delegations.len() != before;
        if !revoke {
            grants.delegations.push(ScratchDelegation {
                family,
                by: by.to_owned(),
                declared_ms: now_ms,
            });
            grants.delegations.sort_by_key(|record| record.family);
            return Ok(true);
        }
        Ok(removed)
    })
}

/// Declare, or withdraw, one legacy claim on one volume.
///
/// A claim is a human's declaration over one scope: the children of one project
/// folder, or the scratch root's ad-hoc entries. Barge validates its shape
/// against the design's units and records it; it is never inferred from a name,
/// a marker's absence or an mtime, and removing it returns the scope to keep and
/// report without touching data.
///
/// A project claim is valid for any project name the design's unit vocabulary
/// allows, including an eight-character name like `underlay`: the claim, not the
/// name, establishes that the folder is a project. Because such a name can also
/// be read as one ad-hoc top-level entry, a project claim for it and a
/// `top_level_adhoc` claim may not overlap, and the second declaration refuses.
pub fn declare_claim(
    state_paths: &StatePaths,
    volume_uuid: &VolumeUuid,
    claim: &ScratchClaim,
    revoke: bool,
) -> Result<GrantsReport> {
    validate_claim(claim)?;
    update_grants(state_paths, volume_uuid, |grants| {
        // The overlap is a property of the recorded set, so it is checked under
        // the same locks the change is published under. A withdrawal is always
        // allowed: it is how an operator resolves an overlap.
        if !revoke {
            refuse_scope_overlap(grants, &claim.scope)?;
        }
        let before = grants.claims.len();
        grants
            .claims
            .retain(|record| !same_scope(&record.scope, &claim.scope));
        let removed = grants.claims.len() != before;
        if revoke {
            return Ok(removed);
        }
        grants.claims.push(claim.clone());
        grants.claims.sort_by_key(|claim| scope_key(&claim.scope));
        Ok(true)
    })
}

/// Refuse a declaration whose scope can name the same top-level directory as an
/// existing claim of the other reading.
///
/// A bare eight-character directory is a project folder only when a
/// `project:<name>` claim names it, and one ad-hoc entry only when a
/// `top_level_adhoc` claim covers it. Both readings at once would be two
/// authorities over one path, so the operator declares one or the other.
fn refuse_scope_overlap(grants: &ScratchGrants, scope: &ClaimScope) -> Result<()> {
    let top_level = grants
        .claims
        .iter()
        .any(|claim| matches!(claim.scope, ClaimScope::TopLevelAdhoc));
    let eight_character_project = grants
        .claims
        .iter()
        .any(|claim| matches!(&claim.scope, ClaimScope::Project { name } if is_run_name(name)));
    let overlap = match scope {
        ClaimScope::Project { name } => top_level && is_run_name(name),
        ClaimScope::TopLevelAdhoc => eight_character_project,
    };
    if !overlap {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Usage,
        "an eight-character top-level directory can be read as a project folder or as one ad-hoc entry, so a project claim and a top_level_adhoc claim may not cover the same name".to_owned(),
    )
    .with_detail("scope", scope_key(scope).into()))
}

fn same_scope(left: &ClaimScope, right: &ClaimScope) -> bool {
    scope_key(left) == scope_key(right)
}

fn scope_key(scope: &ClaimScope) -> String {
    match scope {
        ClaimScope::Project { name } => format!("project:{name}"),
        ClaimScope::TopLevelAdhoc => "top_level_adhoc".to_owned(),
    }
}

/// Validate one claim declaration against the design's unit vocabulary.
fn validate_claim(claim: &ScratchClaim) -> Result<()> {
    if claim.declared_by.trim().is_empty() {
        return Err(Error::new(
            ErrorKind::Usage,
            "a legacy claim must name who declared it".to_owned(),
        ));
    }
    if claim.declared_by.len() > 256 {
        return Err(Error::new(
            ErrorKind::Usage,
            "the declaring name is bounded to 256 bytes".to_owned(),
        ));
    }
    if claim.unmarked_idle_seconds == 0 || claim.unmarked_idle_seconds > MAX_CLAIM_IDLE_SECONDS {
        return Err(Error::new(
            ErrorKind::Usage,
            format!("a claim's idle age is between 1 and {MAX_CLAIM_IDLE_SECONDS} seconds"),
        ));
    }
    let mut seen = BTreeMap::new();
    for family in &claim.families_excluded {
        if seen.insert(*family, ()).is_some() {
            return Err(Error::new(
                ErrorKind::Usage,
                "a claim lists an excluded family only once".to_owned(),
            ));
        }
    }
    match &claim.scope {
        ClaimScope::Project { name } => {
            area::validate_name("scope.name", name)?;
            if claim.unit != ClaimUnit::Child {
                return Err(Error::new(
                    ErrorKind::Usage,
                    "a project claim deletes children, so its unit is `child`".to_owned(),
                ));
            }
            if name == RESERVED_PROJECT {
                return Err(Error::new(
                    ErrorKind::Usage,
                    "the `oxide` project is Oxide's own and can never be claimed".to_owned(),
                ));
            }
            if name.contains('.') {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!(
                        "`{name}` holds a dot, so it is an ad-hoc unit and not a project folder"
                    ),
                ));
            }
        }
        ClaimScope::TopLevelAdhoc => {
            if claim.unit != ClaimUnit::Entry {
                return Err(Error::new(
                    ErrorKind::Usage,
                    "a top-level ad-hoc claim deletes entries, so its unit is `entry`".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// One registry write that changes only scratch grants.
fn update_grants<F>(
    state_paths: &StatePaths,
    volume_uuid: &VolumeUuid,
    change: F,
) -> Result<GrantsReport>
where
    F: FnOnce(&mut ScratchGrants) -> Result<bool>,
{
    // An invalid registry refuses before any effect, and a volume that is not
    // registered is never granted anything. This preflight runs before the layout
    // exists, so a refusal creates no directory, no lock and no file:
    // `state_paths.initialize()` below is the first write.
    match registry::read(state_paths)? {
        registry::Registry::Absent => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "no registry exists, so there is no volume to record a grant on".to_owned(),
            ))
        }
        registry::Registry::Present(document) => {
            if document
                .volumes
                .iter()
                .all(|volume| volume.volume_uuid != *volume_uuid)
            {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!("volume {volume_uuid} is not registered"),
                )
                .with_detail("volume_uuid", volume_uuid.as_str().into()));
            }
        }
    }
    state_paths.initialize()?;
    let mut locks = state::LockSet::new(state_paths.clone());
    let lock = locks.lock_registry()?;
    // Reread under the registry lock and modify *that* document: reading before
    // the lock and publishing the stale object silently discarded a concurrent
    // writer's change, and could restore authority another writer withdrew.
    let mut document = match registry::read(state_paths)? {
        registry::Registry::Present(document) => document,
        registry::Registry::Absent => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "no registry exists, so there is no volume to record a grant on".to_owned(),
            ))
        }
    };
    let Some(slot) = document
        .volumes
        .iter_mut()
        .find(|volume| volume.volume_uuid == *volume_uuid)
    else {
        // A volume that is not registered is never granted anything, and the
        // refusal creates no lock for a volume that does not exist.
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("volume {volume_uuid} is not registered"),
        )
        .with_detail("volume_uuid", volume_uuid.as_str().into()));
    };
    // A grant change is serialized with a deletion's locked authorization
    // window, so it takes the volume lock too, in the documented order. A
    // deletion re-verifies the grant under that same lock, so a withdrawal can
    // never land between the verification and the effect. The document read
    // above stays current while the registry lock is held, so the wait changes
    // no decision.
    let _volume_lock = locks.lock_volume(volume_uuid)?;
    let mut grants = slot.scratch_grants.clone();
    let changed = change(&mut grants)?;
    if !changed {
        return Ok(GrantsReport {
            volume_uuid: volume_uuid.as_str().to_owned(),
            grants,
            changed: false,
        });
    }
    slot.scratch_grants = grants.clone();
    registry::RegistryWriter::new(state_paths).publish(&lock, &document)?;
    Ok(GrantsReport {
        volume_uuid: volume_uuid.as_str().to_owned(),
        grants,
        changed: true,
    })
}

// ---------------------------------------------------------------------------
// The expiry sweep
// ---------------------------------------------------------------------------

/// One sweep's bounds. The design's are 1 s and 4096 examined entries; a test
/// shortens them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepBounds {
    /// How many entries the sweep may examine.
    pub examine: usize,
    /// How long it may spend examining them.
    pub timeout: Duration,
}

impl Default for SweepBounds {
    fn default() -> Self {
        Self {
            examine: SWEEP_EXAMINE,
            timeout: SWEEP_TIMEOUT,
        }
    }
}

/// The policy the plan reports. Scratch has no area declaration and the
/// registry's field set has no scratch knob, so the design's ages are constants
/// and a change to them is a design change, not a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepPolicy {
    /// A native root is expirable this long after its positive conclusion.
    pub native_age_seconds: u64,
    /// A delegated Oxide root is expirable this long after its conclusion.
    pub oxide_age_seconds: u64,
    /// A concluded Effigy run that asked to be kept is retained this long.
    pub effigy_keep_age_seconds: u64,
    /// An Effigy run whose liveness cannot be decided outlives this long.
    pub effigy_age_seconds: u64,
}

impl SweepPolicy {
    /// The design's constants.
    pub const fn design() -> Self {
        Self {
            native_age_seconds: NATIVE_AGE_SECONDS,
            oxide_age_seconds: OXIDE_AGE_SECONDS,
            effigy_keep_age_seconds: EFFIGY_KEEP_AGE_SECONDS,
            effigy_age_seconds: EFFIGY_AGE_SECONDS,
        }
    }
}

/// Which marker family identifies a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitFamily {
    /// A `barge.root/1` scratch run Barge created.
    Native,
    /// An `.oxide-scratch.json` root, concluded by Oxide's supervisor.
    OxideScratch,
    /// An `.effigy-scratch.json` directory, concluded by Effigy.
    EffigyScratch,
    /// No marker of any recognised family.
    Unmarked,
}

impl UnitFamily {
    /// The family's name as a document reports it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::OxideScratch => "oxide_scratch",
            Self::EffigyScratch => "effigy_scratch",
            Self::Unmarked => "unmarked",
        }
    }

    /// The recorded grant a unit of this family needs.
    pub const fn grant(self) -> &'static str {
        match self {
            Self::Native => "native_root",
            Self::OxideScratch | Self::EffigyScratch => "delegation",
            Self::Unmarked => "legacy_claim",
        }
    }
}

/// What a unit's scope is: one project folder's children, or the scratch root's
/// ad-hoc entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnitScope {
    /// A child of `<scratch>/<name>/`.
    Project {
        /// The project folder's name.
        name: String,
    },
    /// An entry directly under the scratch root.
    TopLevel,
}

impl UnitScope {
    /// The scope as a document reports it, and as a sorted key.
    pub fn key(&self) -> String {
        match self {
            Self::Project { name } => format!("project:{name}"),
            Self::TopLevel => "top_level".to_owned(),
        }
    }

    /// The folder string this scope's units live in.
    fn parent_folder(&self, scratch: &str) -> String {
        match self {
            Self::Project { name } => project_folder(scratch, name),
            Self::TopLevel => scratch.to_owned(),
        }
    }

    /// The unit shape a claim for this scope must declare.
    const fn claim_unit(&self) -> ClaimUnit {
        match self {
            Self::Project { .. } => ClaimUnit::Child,
            Self::TopLevel => ClaimUnit::Entry,
        }
    }
}

/// One candidate the plan would delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRow {
    /// The unit's name inside its folder.
    pub name: String,
    /// The scope that owns the unit.
    pub scope: UnitScope,
    /// The marker family that identifies it.
    pub family: UnitFamily,
    /// The unit's absolute path, as the walk reached it.
    pub path: PathBuf,
    /// The observed unit kind, bound into the plan digest.
    pub kind: UnitKind,
    /// The unit's device.
    pub device: u64,
    /// The unit's inode on that device.
    pub inode: u64,
    /// The family marker file, `None` for an unmarked unit.
    pub marker_file: Option<String>,
    /// The SHA-256 identity a delete records: the family marker's bytes, or the
    /// provenance digest of an unmarked unit.
    pub marker_digest: String,
    /// When the family concluded the unit, when it recorded one.
    pub concluded_ms: Option<u64>,
    /// When the unit was created, when the family recorded it.
    pub created_ms: Option<u64>,
    /// The newest activity the plan read: the newest entry of a claimed unit, or
    /// the family's own timestamp.
    pub last_activity_ms: u64,
    /// The recorded grant that covers it.
    pub grant: String,
    /// Why it is eligible, in one line.
    pub reason: String,
}

/// One unit the plan keeps, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeptRow {
    /// The unit's or entry's name.
    pub name: String,
    /// The scope it was found in, or `top_level` for a reserved entry.
    pub scope: String,
    /// The absolute path, when it was opened.
    pub path: Option<PathBuf>,
    /// What it is: a family, `unmarked`, `symlink` or a reserved name.
    pub state: String,
    /// Why it is kept.
    pub reason: String,
}

/// The `barge.scratch.plan/1` body: the stable plan and nothing volatile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepPlan {
    /// Always `barge.scratch.plan/1`.
    pub schema: String,
    /// The registered volume.
    pub volume_uuid: VolumeUuid,
    /// The volume's proved mount point.
    pub mount_point: PathBuf,
    /// The volume's recorded `root_dir`.
    pub root_dir: String,
    /// The scratch folder the volume resolves.
    pub folder: String,
    /// The ages the plan applied.
    pub policy: SweepPolicy,
    /// The candidates, oldest first.
    pub candidates: RowSet<PlanRow>,
    /// The units kept, with their reasons.
    pub kept: RowSet<KeptRow>,
    /// How many entries the sweep examined.
    pub examined: usize,
    /// Whether a bound stopped the sweep before the folder was exhausted.
    pub truncated: bool,
}

/// The plan's own schema string.
pub const PLAN_SCHEMA: &str = "barge.scratch.plan/1";

/// One unit a real run deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletedRow {
    /// The unit's name.
    pub name: String,
    /// The scope it was found in.
    pub scope: UnitScope,
    /// The family it belonged to.
    pub family: UnitFamily,
    /// Its path before the deletion.
    pub path: PathBuf,
    /// The intent that recorded it.
    pub intent_id: String,
    /// How many entries the deletion removed.
    pub entries_removed: usize,
    /// How many directories it removed, the quarantine leaf included.
    pub directories_removed: usize,
    /// Whether a marker was unlinked last.
    pub marker_removed: bool,
}

/// One candidate the real run did not delete, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedRow {
    /// The unit's name.
    pub name: String,
    /// The scope it was found in.
    pub scope: UnitScope,
    /// Why it was kept this run.
    pub reason: String,
}

/// The `barge.scratch.expire/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpireReport {
    /// The registered volume.
    pub volume_uuid: String,
    /// Whether the volume was attached. An ephemeral volume's absence is
    /// reported and skipped, never an error.
    pub volume_present: bool,
    /// Whether this run only previewed.
    pub preview: bool,
    /// The SHA-256 of the plan's stable bytes.
    pub plan_digest: String,
    /// The stable plan.
    pub plan: SweepPlan,
    /// The units this run deleted.
    pub deleted: RowSet<DeletedRow>,
    /// The intents this run left pending.
    pub pending: RowSet<PendingRow>,
    /// The candidates this run did not delete.
    pub skipped: RowSet<SkippedRow>,
    /// What a real run's replay of earlier pending intents did. A preview never
    /// replays, so this is `None` there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<delete::ReplayReport>,
}

impl ExpireReport {
    /// Whether the run left an intent that a later run must replay.
    ///
    /// A run can create no new intent and still leave work unsettled: its replay
    /// of an earlier intent may have stopped short, and `replay.pending` names
    /// that. The command's unsettled exit uses this, so a pending replay is never
    /// reported as success.
    pub fn has_pending(&self) -> bool {
        self.pending.count > 0
            || self
                .replay
                .as_ref()
                .is_some_and(|replay| !replay.is_settled())
    }
}

/// What one caller asks `barge scratch expire` for.
#[derive(Debug, Clone)]
pub struct SweepRequest<'a> {
    /// The registered volume to sweep.
    pub volume_uuid: &'a VolumeUuid,
    /// Whether to preview only. A preview changes nothing.
    pub preview: bool,
    /// The digest a real run is authorized by. A real run without it refuses.
    pub plan_digest: Option<&'a str>,
    /// The sweep's bounds.
    pub bounds: SweepBounds,
    /// The deletion's bounds.
    pub delete_bounds: DeleteBounds,
    /// The current time, so a test can age a conclusion.
    pub now_ms: u64,
}

/// Sweep one volume's scratch folder: preview, or delete exactly the previewed
/// plan.
///
/// A preview computes the plan and its SHA-256 and changes nothing. A real run
/// requires that digest, recomputes the plan, refuses when it differs, and then
/// re-verifies every predicate immediately before each effect. Deletion is S5's:
/// custody, the intent journal, the quarantine rename and whole-unit delete.
#[allow(clippy::too_many_arguments)]
pub fn sweep(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &SweepRequest<'_>,
) -> Result<ExpireRun> {
    if !request.preview {
        let Some(digest) = request.plan_digest else {
            return Err(Error::new(
                ErrorKind::Usage,
                "a real expiry needs --plan-sha256 from a preview; nothing was deleted".to_owned(),
            ));
        };
        if !is_sha256(digest) {
            return Err(Error::new(
                ErrorKind::Usage,
                "the plan digest is not a SHA-256 hex string".to_owned(),
            )
            .with_detail("plan_digest", digest.into()));
        }
    }
    let record = area::registered_volume(state_paths, request.volume_uuid)?;
    record.declared_durability()?;

    // A volume that is not attached is reported, not treated as empty: an
    // ephemeral volume is expected to be away sometimes, so its absence is a
    // report; a permanent volume's absence is loud, because it is abnormal.
    if let Err(error) = area::VolumeRoot::open(dirs, facts, mounts, &record) {
        if error.kind() != ErrorKind::VolumeAbsent {
            return Err(error);
        }
        if record.availability == registry::Availability::Permanent {
            return Err(Error::new(
                ErrorKind::VolumeAbsent,
                format!(
                    "volume {} is permanent and is not attached; nothing was swept and its stale mount point was not written",
                    record.volume_uuid
                ),
            )
            .with_detail("volume_uuid", record.volume_uuid.as_str().into())
            .with_detail("mount_hint", record.mount_hint.clone().map_or_else(
                || serde_json::Value::Null,
                |path| serde_json::Value::String(path.display().to_string()),
            )));
        }
        let plan = empty_plan(&record);
        let digest = plan_digest(&plan)?;
        return Ok(ExpireRun {
            report: ExpireReport {
                volume_uuid: record.volume_uuid.as_str().to_owned(),
                volume_present: false,
                preview: request.preview,
                plan_digest: digest,
                plan,
                deleted: RowSet::empty(),
                pending: RowSet::empty(),
                skipped: RowSet::empty(),
                replay: None,
            },
            timed_out: false,
            unreached: 0,
            stopped: None,
        });
    }

    let plan = build_plan(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        &record,
        request.now_ms,
        request.bounds,
    )?;
    let digest = plan_digest(&plan)?;
    if request.preview {
        return Ok(ExpireRun {
            report: ExpireReport {
                volume_uuid: record.volume_uuid.as_str().to_owned(),
                volume_present: true,
                preview: true,
                plan_digest: digest,
                plan,
                deleted: RowSet::empty(),
                pending: RowSet::empty(),
                skipped: RowSet::empty(),
                replay: None,
            },
            timed_out: false,
            unreached: 0,
            stopped: None,
        });
    }
    if request.plan_digest != Some(digest.as_str()) {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the scratch plan changed since the preview; preview again before deleting".to_owned(),
        )
        .with_detail(
            "expected_plan_digest",
            request.plan_digest.unwrap_or("").into(),
        )
        .with_detail("actual_plan_digest", digest.into())
        .with_detail("candidates", plan.candidates.count.into()));
    }

    expire_plan(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &plan,
        record.generation,
        request.delete_bounds,
        request.now_ms,
        None,
    )
}

/// Apply one `barge.scratch.plan/1` exactly as it stands, re-verifying each unit.
///
/// This is `barge maintain`'s entry point: the maintain plan carries S4's own
/// scratch plan, and a real maintain run applies those rows rather than
/// recomputing and comparing a whole new plan. Each candidate is re-read
/// immediately before its effect (`reassess` and the deletion's own
/// authorization), so a unit that became ineligible — a live hold, a withdrawn
/// delegation or claim, a marker that appeared, a changed identity or a moved
/// configuration — is kept and reported as that unit only, and the other planned
/// units still go. `sweep` keeps its own contract, which recomputes the plan and
/// refuses the whole run when the digest differs.
///
/// The digest recorded in the report and in each intent is the digest of the
/// plan's own bytes, so the journal can always be re-derived from the plan the
/// caller presented.
///
/// `deadline` is the caller's wall clock. When it is reached, no further effect
/// starts: the intent replay is skipped, every planned candidate that was not
/// attempted is reported in `skipped` with the wall-clock reason, and
/// [`ExpireRun::timed_out`] says so even when the effect that pushed past it had
/// already completed. An effect that was already running when the clock was
/// reached is never lost from the accounting: it stays in `deleted` or `pending`,
/// and only the candidates after it count as unreached.
#[allow(clippy::too_many_arguments)]
pub fn expire_plan(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    plan: &SweepPlan,
    expected_generation: u64,
    delete_bounds: DeleteBounds,
    now_ms: u64,
    deadline: Option<Instant>,
) -> Result<ExpireRun> {
    if plan.volume_uuid != record.volume_uuid {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the plan is for volume {}, not the verified {}",
                plan.volume_uuid, record.volume_uuid
            ),
        )
        .with_detail("plan_volume_uuid", plan.volume_uuid.as_str().into())
        .with_detail("volume_uuid", record.volume_uuid.as_str().into()));
    }
    let digest = plan_digest(plan)?;

    // Only an expendable volume may be pruned, and only under the record the plan
    // was made for. A record that now says `kept` (or declares nothing), or that
    // has been written since the plan, keeps every planned unit: applying a plan
    // made under another record would delete data the volume no longer authorizes.
    if record.generation != expected_generation {
        let reason = format!(
            "the volume record changed since the plan (generation {expected_generation} to {}), so the planned unit is kept",
            record.generation
        );
        return Ok(kept_plan_run(record, plan, &digest, &reason));
    }
    if !record.durability.permits_pruning() {
        let reason = format!(
            "the volume now declares `{}`, which never permits pruning, so the planned unit is kept",
            durability_name(record.durability)
        );
        return Ok(kept_plan_run(record, plan, &digest, &reason));
    }
    let root_dir = plan.root_dir.clone();

    // A volume that went away between the plan and this run is reported: every
    // planned unit is kept, and the run continues with the other volumes.
    if let Err(error) = area::VolumeRoot::open(dirs, facts, mounts, record) {
        if error.kind() != ErrorKind::VolumeAbsent {
            return Err(error);
        }
        let skipped: Vec<SkippedRow> = plan
            .candidates
            .rows
            .iter()
            .map(|row| SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: format!(
                    "the volume {} is not attached, so the planned unit is kept",
                    record.volume_uuid
                ),
            })
            .collect();
        let total = skipped.len();
        return Ok(ExpireRun {
            report: ExpireReport {
                volume_uuid: record.volume_uuid.as_str().to_owned(),
                volume_present: false,
                preview: false,
                plan_digest: digest,
                plan: plan.clone(),
                deleted: RowSet::empty(),
                pending: RowSet::empty(),
                skipped: RowSet::bounded(skipped, total, ROOT_ROWS),
                replay: None,
            },
            timed_out: false,
            unreached: 0,
            stopped: None,
        });
    }

    let scratch = plan.folder.clone();
    let mut deleted: Vec<DeletedRow> = Vec::new();
    let mut pending: Vec<PendingRow> = Vec::new();
    let mut skipped: Vec<SkippedRow> = Vec::new();
    let mut timed_out = false;
    let mut unreached = 0usize;
    let mut stopped: Option<String> = None;
    // A real run first settles what an earlier run left pending, from the
    // recorded identity only, so the effects below are not blocked by work the
    // run already knows how to finish. A preview never replays, and a run whose
    // clock is already spent starts no replay either.
    let replay = if expired(deadline) {
        timed_out = true;
        None
    } else {
        Some(delete::replay_pending(
            state_paths,
            dirs,
            facts,
            observer,
            delete_bounds,
            deadline,
        )?)
    };
    // A replay that ran to the caller's clock — or a stage reached with an empty
    // candidate list — is still a timeout: the run overran its bound and must
    // never be reported as a success, and no later effect starts.
    if expired(deadline) {
        timed_out = true;
    }
    let mut intent_seen = false;
    for row in plan.candidates.rows.iter() {
        if let Some(reason) = &stopped {
            skipped.push(SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: format!("the stage stopped before this unit: {reason}"),
            });
            continue;
        }
        if intent_seen {
            skipped.push(SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: "a pending intent stopped this sweep; a later run replays it".to_owned(),
            });
            continue;
        }
        // The caller's wall clock stops every further effect; what it did not
        // reach is reported with counts rather than left out.
        if timed_out || expired(deadline) {
            timed_out = true;
            unreached = unreached.saturating_add(1);
            skipped.push(SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: WALL_CLOCK_REASON.to_owned(),
            });
            continue;
        }
        // Every predicate is re-read here, immediately before the effect.
        let fresh = match reassess(
            dirs,
            facts,
            mounts,
            observer,
            state_paths,
            &record.volume_uuid,
            &row.scope,
            &row.name,
            now_ms,
        ) {
            Ok(fresh) => fresh,
            Err(error) if is_per_unit_refusal(&error) => {
                skipped.push(SkippedRow {
                    name: row.name.clone(),
                    scope: row.scope.clone(),
                    reason: error.to_string(),
                });
                continue;
            }
            Err(error) => {
                skipped.push(SkippedRow {
                    name: row.name.clone(),
                    scope: row.scope.clone(),
                    reason: format!("the unit could not be verified again: {error}"),
                });
                stopped = Some(format!(
                    "the stage stopped while re-verifying {}: {error}",
                    row.name
                ));
                continue;
            }
        };
        let Some(fresh) = fresh else {
            skipped.push(SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: "the unit is no longer eligible, so the effect was abandoned".to_owned(),
            });
            continue;
        };
        let observed = fresh.observed;
        if fresh.row.family != row.family
            || fresh.row.kind != row.kind
            || observed.marker_digest != row.marker_digest
            || observed.device.get() != row.device
            || observed.inode != row.inode
        {
            skipped.push(SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: "the unit's identity changed since the plan, so the effect was abandoned"
                    .to_owned(),
            });
            continue;
        }
        let declaration = scratch_declaration(record, &scratch);
        let (parent, claim) = match &row.scope {
            UnitScope::Project { name } => (name.clone(), format!("project:{name}")),
            UnitScope::TopLevel => (String::new(), "top_level_adhoc".to_owned()),
        };
        let (marker, grant) = match row.family {
            UnitFamily::Native => (
                Some(UnitMarker::new(
                    UnitMarkerFamily::Barge,
                    row.marker_digest.clone(),
                )),
                UnitGrant::native_root(row.name.clone()),
            ),
            UnitFamily::OxideScratch => (
                Some(UnitMarker::new(
                    UnitMarkerFamily::OxideScratch,
                    row.marker_digest.clone(),
                )),
                UnitGrant::delegation(UnitMarkerFamily::OxideScratch),
            ),
            UnitFamily::EffigyScratch => (
                Some(UnitMarker::new(
                    UnitMarkerFamily::EffigyScratch,
                    row.marker_digest.clone(),
                )),
                UnitGrant::delegation(UnitMarkerFamily::EffigyScratch),
            ),
            UnitFamily::Unmarked => (None, UnitGrant::claim(claim)),
        };
        // Re-verification above reads markers, holds and directories and can be
        // slow, so the clock is checked again here, at the effect boundary: an
        // effect never starts inside the stop window because the check that
        // preceded it was cheap.
        if expired(deadline) {
            timed_out = true;
            unreached = unreached.saturating_add(1);
            skipped.push(SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: WALL_CLOCK_REASON.to_owned(),
            });
            continue;
        }
        let kind = row.kind;
        let unit = DeletionUnit {
            volume_uuid: record.volume_uuid.clone(),
            root_dir: root_dir.clone(),
            folder: scratch.clone(),
            parent,
            name: row.name.clone(),
            kind,
            device: observed.device,
            inode: observed.inode,
            marker,
            grant,
            plan_digest: digest.clone(),
        };
        let authorization = |fresh: &VolumeRecord| {
            authorize_under(
                dirs,
                facts,
                mounts,
                observer,
                state_paths,
                fresh,
                expected_generation,
                row,
                now_ms,
            )
        };
        match delete::delete_unit_authorized(
            state_paths,
            dirs,
            facts,
            mounts,
            observer,
            &declaration,
            None,
            &unit,
            delete_bounds,
            Some(&authorization),
            &crate::state::IntentEvidence::none(),
        ) {
            Ok(report) => {
                if report.is_deleted() {
                    deleted.push(DeletedRow {
                        name: row.name.clone(),
                        scope: row.scope.clone(),
                        family: row.family,
                        path: row.path.clone(),
                        intent_id: report.intent_id.clone(),
                        entries_removed: report.entries_removed,
                        directories_removed: report.directories_removed,
                        marker_removed: report.marker_removed,
                    });
                } else {
                    intent_seen = true;
                    pending.push(PendingRow {
                        intent_id: report.intent_id.clone(),
                        unit_name: report.unit_name.clone(),
                        unit_kind: report.unit_kind.clone(),
                        phase: report.phase,
                        reason: report
                            .reason
                            .clone()
                            .unwrap_or_else(|| "the deletion stopped short".to_owned()),
                    });
                }
            }
            Err(error) if is_per_unit_refusal(&error) => {
                skipped.push(SkippedRow {
                    name: row.name.clone(),
                    scope: row.scope.clone(),
                    reason: error.to_string(),
                });
            }
            Err(error) => {
                skipped.push(SkippedRow {
                    name: row.name.clone(),
                    scope: row.scope.clone(),
                    reason: format!("the unit's effect failed: {error}"),
                });
                stopped = Some(format!(
                    "the stage stopped while deleting {}: {error}",
                    row.name
                ));
                continue;
            }
        }
        // An effect that finished past the caller's clock is still this run's
        // work — it is in `deleted` or `pending` — but the run did not finish
        // inside its bound, so it is a timeout and no later effect starts.
        if expired(deadline) {
            timed_out = true;
        }
    }
    // A stage whose last candidate was re-verified and then skipped — refused,
    // no longer eligible, or its identity changed — still crossed the clock, so
    // the stage reports the timeout instead of a success.
    if expired(deadline) {
        timed_out = true;
    }
    let deleted_total = deleted.len();
    let pending_total = pending.len();
    let skipped_total = skipped.len();
    Ok(ExpireRun {
        report: ExpireReport {
            volume_uuid: record.volume_uuid.as_str().to_owned(),
            volume_present: true,
            preview: false,
            plan_digest: digest,
            plan: plan.clone(),
            deleted: RowSet::bounded(deleted, deleted_total, ROOT_ROWS),
            pending: RowSet::bounded(pending, pending_total, ROOT_ROWS),
            skipped: RowSet::bounded(skipped, skipped_total, ROOT_ROWS),
            replay,
        },
        timed_out,
        unreached,
        stopped,
    })
}

/// A durability's name, as a report spells it.
const fn durability_name(durability: registry::Durability) -> &'static str {
    match durability {
        registry::Durability::Expendable => "expendable",
        registry::Durability::Kept => "kept",
        registry::Durability::Undeclared => "undeclared",
    }
}

/// The reason one planned scratch unit was not attempted: the caller's wall
/// clock was reached before its effect.
const WALL_CLOCK_REASON: &str =
    "the run's wall clock was reached before this effect, so the planned unit is kept";

/// Whether a caller's deadline has been reached. `None` is no deadline.
fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// What one application of a plan did, and what the caller's deadline left
/// undone. `report` is S4's own document; the other two fields are the maintain
/// run's accounting, so a stage that ran out of clock is never reported as a
/// complete run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpireRun {
    /// The `barge.scratch.expire/1` report for the stage.
    pub report: ExpireReport,
    /// Whether the caller's deadline was reached: before an effect (so it never
    /// started), or while one was running (so the run overran its bound).
    pub timed_out: bool,
    /// How many planned candidates were never attempted because of it.
    pub unreached: usize,
    /// Why the stage stopped before its last candidate, if it did: a failure that
    /// is not one unit's refusal. The rows above are the work already done.
    pub stopped: Option<String>,
}

/// A refusal that concerns one unit and must not abort the sweep or a maintain
/// stage. Slice S7's area prune reuses the same set, so there is one list.
pub(crate) fn is_per_unit_refusal(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::CustodyNotProven
            | ErrorKind::ForeignOwner
            | ErrorKind::RefusedContainment
            | ErrorKind::RefusedBoundary
            | ErrorKind::ResourceBound
            | ErrorKind::Locked
    )
}

/// The SHA-256 of one plan's stable bytes.
pub fn plan_digest(plan: &SweepPlan) -> Result<String> {
    let bytes = serde_json::to_vec(plan).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the sweep plan: {error}"),
        )
    })?;
    if bytes.len() > crate::contract::DOCUMENT_SIZE_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "the sweep plan is over the document cap".to_owned(),
        ));
    }
    Ok(sha256_hex(&bytes))
}

/// Every planned unit kept, with one reason: the record no longer authorizes
/// pruning this plan.
fn kept_plan_run(record: &VolumeRecord, plan: &SweepPlan, digest: &str, reason: &str) -> ExpireRun {
    let skipped: Vec<SkippedRow> = plan
        .candidates
        .rows
        .iter()
        .map(|row| SkippedRow {
            name: row.name.clone(),
            scope: row.scope.clone(),
            reason: reason.to_owned(),
        })
        .collect();
    let total = skipped.len();
    ExpireRun {
        report: ExpireReport {
            volume_uuid: record.volume_uuid.as_str().to_owned(),
            volume_present: true,
            preview: false,
            plan_digest: digest.to_owned(),
            plan: plan.clone(),
            deleted: RowSet::empty(),
            pending: RowSet::empty(),
            skipped: RowSet::bounded(skipped, total, ROOT_ROWS),
            replay: None,
        },
        timed_out: false,
        unreached: 0,
        stopped: None,
    }
}

fn empty_plan(record: &VolumeRecord) -> SweepPlan {
    SweepPlan {
        schema: PLAN_SCHEMA.to_owned(),
        volume_uuid: record.volume_uuid.clone(),
        mount_point: record.mount_hint.clone().unwrap_or_default(),
        root_dir: record.root_dir.clone(),
        folder: scratch_folder(record),
        policy: SweepPolicy::design(),
        candidates: RowSet::empty(),
        kept: RowSet::empty(),
        examined: 0,
        truncated: false,
    }
}

/// The sweep's own budget: examined entries and a deadline.
struct Budget {
    deadline: Instant,
    remaining: usize,
    examined: usize,
    truncated: bool,
}

impl Budget {
    fn new(bounds: SweepBounds) -> Self {
        Self {
            deadline: Instant::now() + bounds.timeout,
            remaining: bounds.examine.max(1),
            examined: 0,
            truncated: false,
        }
    }

    /// Account for one examined entry. `Ok(false)` means the sweep must stop
    /// here and report the rest as unreached.
    fn spend(&mut self, added: usize) -> bool {
        if self.remaining < added || Instant::now() >= self.deadline {
            self.truncated = true;
            return false;
        }
        self.remaining -= added;
        self.examined = self.examined.saturating_add(added);
        true
    }
}

/// The facts a classification needs.
struct SweepContext<'a> {
    dirs: &'a dyn DirOps,
    observer: &'a dyn ProcessObserver,
    state_paths: &'a StatePaths,
    record: &'a VolumeRecord,
    device: DeviceId,
    now_ms: u64,
    grants: &'a ScratchGrants,
    policy: SweepPolicy,
}

impl SweepContext<'_> {
    fn has_delegation(&self, family: MarkerFamily) -> bool {
        self.grants
            .delegations
            .iter()
            .any(|delegation| delegation.family == family)
    }

    fn claim_for(&self, scope: &UnitScope) -> Option<&ScratchClaim> {
        self.grants
            .claims
            .iter()
            .find(|claim| match (&claim.scope, scope) {
                (ClaimScope::Project { name }, UnitScope::Project { name: unit }) => name == unit,
                (ClaimScope::TopLevelAdhoc, UnitScope::TopLevel) => true,
                _ => false,
            })
    }
}

/// Re-verify one planned unit under the registry record the deletion read in its
/// locked window.
///
/// The whole current policy is re-evaluated, not just the grant's presence: the
/// family's delegation or the scope's claim must still cover the unit, the
/// claim's declared unit shape must still match, and the claim's own idle age
/// must still make the unit eligible. A claim replaced by a stricter one between
/// the plan and the effect therefore refuses the unit (`refused_boundary`, the
/// sweep's per-unit refusal) instead of authorizing it under the old policy; the
/// recorded identity must not have changed either.
#[allow(clippy::too_many_arguments)]
fn authorize_under(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    record: &VolumeRecord,
    expected_generation: u64,
    row: &PlanRow,
    now_ms: u64,
) -> Result<()> {
    // The record is re-read under the volume lock and the plan was made under one
    // record: a durability that no longer permits pruning, or any registry write
    // since the plan, keeps this unit. The snapshot the run started with is not
    // enough, because either can change while earlier units are deleted.
    if !record.durability.permits_pruning() {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the volume now declares `{}`, which never permits pruning, so the unit is kept",
                durability_name(record.durability)
            ),
        )
        .with_detail("volume_uuid", record.volume_uuid.as_str().into()));
    }
    if record.generation != expected_generation {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the volume record changed since the plan (generation {expected_generation} to {}), so the unit is kept",
                record.generation
            ),
        )
        .with_detail("volume_uuid", record.volume_uuid.as_str().into()));
    }
    let Some(current) = reassess_under(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        record,
        &row.scope,
        &row.name,
        now_ms,
    )?
    else {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "`{}` is no longer eligible under the record's current grant and policy, so the effect was abandoned",
                row.scope.key()
            ),
        )
        .with_detail("scope", row.scope.key().into())
        .with_detail("family", row.family.as_str().into()));
    };
    if current.row.family != row.family
        || current.row.kind != row.kind
        || current.observed.marker_digest != row.marker_digest
        || current.observed.device.get() != row.device
        || current.observed.inode != row.inode
    {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "`{}` is not the identity the plan recorded, so the effect was abandoned",
                row.scope.key()
            ),
        )
        .with_detail("scope", row.scope.key().into()));
    }
    Ok(())
}

/// One unit as the classifier decided it.
struct Assessment {
    row: PlanRow,
    observed: ObservedRoot,
}

/// One classification result.
enum AssessmentOutcome {
    Eligible(Box<Assessment>),
    Kept { state: String, reason: String },
}

enum Assessed {
    Eligible(Box<PlanRow>, ObservedRoot),
    Kept { state: String, reason: String },
}

/// Build the stable plan for one volume. Nothing is written or deleted.
#[allow(clippy::too_many_arguments)]
fn build_plan(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    record: &VolumeRecord,
    now_ms: u64,
    bounds: SweepBounds,
) -> Result<SweepPlan> {
    let scratch = scratch_folder(record);
    let policy = SweepPolicy::design();
    let mut plan = SweepPlan {
        schema: PLAN_SCHEMA.to_owned(),
        volume_uuid: record.volume_uuid.clone(),
        mount_point: record.mount_hint.clone().unwrap_or_default(),
        root_dir: record.root_dir.clone(),
        folder: scratch.clone(),
        policy,
        candidates: RowSet::empty(),
        kept: RowSet::empty(),
        examined: 0,
        truncated: false,
    };
    let Some((path, handle)) = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &record.root_dir,
        &scratch,
    )?
    else {
        return Ok(plan);
    };
    plan.mount_point = path.clone();
    let mut budget = Budget::new(bounds);
    let mut candidates: Vec<PlanRow> = Vec::new();
    let mut kept: Vec<KeptRow> = Vec::new();
    let ctx = SweepContext {
        dirs,
        observer,
        state_paths,
        record,
        device: handle.identity().device,
        now_ms,
        grants: &record.scratch_grants,
        policy,
    };
    let listing = dirs.list_children(&handle, LISTING_NAME_ROWS)?;
    let mut entries = listing.entries.clone();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in &entries {
        if !budget.spend(1) {
            break;
        }
        classify_top_level(
            &ctx,
            &handle,
            &path,
            entry,
            &mut budget,
            &mut candidates,
            &mut kept,
        )?;
    }
    candidates.sort_by(|left, right| {
        (left.last_activity_ms, left.scope.key(), left.name.clone()).cmp(&(
            right.last_activity_ms,
            right.scope.key(),
            right.name.clone(),
        ))
    });
    kept.sort_by(|left, right| {
        (left.scope.clone(), left.name.clone()).cmp(&(right.scope.clone(), right.name.clone()))
    });
    let candidate_total = candidates.len();
    let kept_total = kept.len();
    plan.candidates = RowSet::bounded(candidates, candidate_total, ROOT_ROWS);
    plan.kept = RowSet::bounded(kept, kept_total, ROOT_ROWS);
    plan.examined = budget.examined;
    plan.truncated = budget.truncated || listing.truncated;
    Ok(plan)
}

/// Classify one entry directly under the scratch root.
fn classify_top_level(
    ctx: &SweepContext<'_>,
    handle: &DirHandle,
    path: &Path,
    entry: &crate::os::ChildEntry,
    budget: &mut Budget,
    candidates: &mut Vec<PlanRow>,
    kept: &mut Vec<KeptRow>,
) -> Result<()> {
    let unit_path = path.join(&entry.name);
    if entry.name.starts_with(state::QUARANTINE_PREFIX) {
        kept.push(kept_row(
            &entry.name,
            "top_level",
            Some(unit_path),
            "quarantine",
            "a pending deletion's quarantine leaf; only a replay settles it",
        ));
        return Ok(());
    }
    if entry.facts.kind == EntryKind::Symlink {
        kept.push(kept_row(
            &entry.name,
            "top_level",
            Some(unit_path),
            "symlink",
            "a symbolic link is never followed or deleted",
        ));
        return Ok(());
    }
    if entry.facts.kind == EntryKind::Other {
        kept.push(kept_row(
            &entry.name,
            "top_level",
            Some(unit_path),
            "other",
            "neither a file nor a directory",
        ));
        return Ok(());
    }
    if entry.facts.kind == EntryKind::Directory {
        if entry.name == OXIDE_VALIDATION_DIR {
            kept.push(kept_row(
                &entry.name,
                "top_level",
                Some(unit_path),
                "oxide_validation",
                "Oxide's validation temp is never a Barge unit and no claim may cover it",
            ));
            return Ok(());
        }
        // A marked directory at the scratch root is a unit (an Effigy run
        // folder, or an Oxide or Barge root), checked before any name-shaped
        // rule: a name never establishes ownership.
        let family = unit_family(ctx, handle, &entry.name, &unit_path)?;
        if family != UnitFamily::Unmarked {
            return classify_unit(
                ctx,
                handle,
                &entry.name,
                &unit_path,
                &UnitScope::TopLevel,
                family,
                budget,
                candidates,
                kept,
            );
        }
        if entry.name == RESERVED_PROJECT {
            // Oxide's own project: marked family units inside it are still
            // classified (a delegation covers them), and nothing unmarked there
            // is ever claimed.
            kept.push(kept_row(
                &entry.name,
                "top_level",
                Some(unit_path.clone()),
                "reserved_project",
                "the `oxide` project is Oxide's own; only a marked family unit inside it is ever expired",
            ));
            return classify_project(
                ctx,
                handle,
                &entry.name,
                &unit_path,
                Reserved::MarkedOnly("the `oxide` project is Oxide's own"),
                budget,
                candidates,
                kept,
            );
        }
        if is_run_name(&entry.name) {
            // A bare eight-character name is a run name by shape, and a name never
            // establishes ownership. It is a project folder only when an
            // explicit `project:<name>` claim names it, and one ad-hoc entry only
            // when a `top_level_adhoc` claim covers it; the two claims may not
            // overlap. A marked family unit inside it is always classified.
            if ctx
                .claim_for(&UnitScope::Project {
                    name: entry.name.clone(),
                })
                .is_some()
            {
                return classify_project(
                    ctx,
                    handle,
                    &entry.name,
                    &unit_path,
                    Reserved::No,
                    budget,
                    candidates,
                    kept,
                );
            }
            if ctx.claim_for(&UnitScope::TopLevel).is_some() {
                return classify_unit(
                    ctx,
                    handle,
                    &entry.name,
                    &unit_path,
                    &UnitScope::TopLevel,
                    UnitFamily::Unmarked,
                    budget,
                    candidates,
                    kept,
                );
            }
            kept.push(kept_row(
                &entry.name,
                "top_level",
                Some(unit_path.clone()),
                "run_name_like",
                "a bare eight-character name is a project folder only under an explicit project claim and an ad-hoc entry only under a top_level_adhoc claim",
            ));
            return classify_project(
                ctx,
                handle,
                &entry.name,
                &unit_path,
                Reserved::MarkedOnly(
                    "a bare eight-character name is never claimed without a project claim",
                ),
                budget,
                candidates,
                kept,
            );
        }
        if entry.name.contains('.') {
            // A dotted directory is one ad-hoc unit, not a project folder.
            return classify_unit(
                ctx,
                handle,
                &entry.name,
                &unit_path,
                &UnitScope::TopLevel,
                UnitFamily::Unmarked,
                budget,
                candidates,
                kept,
            );
        }
        return classify_project(
            ctx,
            handle,
            &entry.name,
            &unit_path,
            Reserved::No,
            budget,
            candidates,
            kept,
        );
    }
    // A file directly under the scratch root is one ad-hoc unit.
    classify_unit(
        ctx,
        handle,
        &entry.name,
        &unit_path,
        &UnitScope::TopLevel,
        UnitFamily::Unmarked,
        budget,
        candidates,
        kept,
    )
}

/// Whether a project folder is one Barge never claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reserved {
    /// A claim may cover its children.
    No,
    /// Only a marked family unit is classified; every unmarked child is kept
    /// with this reason.
    MarkedOnly(&'static str),
}

/// Classify one project folder's children.
#[allow(clippy::too_many_arguments)]
fn classify_project(
    ctx: &SweepContext<'_>,
    scratch_handle: &DirHandle,
    project: &str,
    project_path: &Path,
    reserved: Reserved,
    budget: &mut Budget,
    candidates: &mut Vec<PlanRow>,
    kept: &mut Vec<KeptRow>,
) -> Result<()> {
    let project_handle = match ctx.dirs.open_child_dir(scratch_handle, project) {
        Ok(handle) => handle,
        Err(error) => {
            kept.push(kept_row(
                project,
                "top_level",
                Some(project_path.to_path_buf()),
                "unreadable",
                &format!("the project folder cannot be opened: {error}"),
            ));
            return Ok(());
        }
    };
    let listing = match ctx.dirs.list_children(&project_handle, LISTING_NAME_ROWS) {
        Ok(listing) => listing,
        Err(error) => {
            kept.push(kept_row(
                project,
                "top_level",
                Some(project_path.to_path_buf()),
                "unreadable",
                &format!("the project folder cannot be listed: {error}"),
            ));
            return Ok(());
        }
    };
    // A project folder whose listing stopped at the name bound was not fully
    // examined, so the plan says so instead of presenting a partial plan as whole.
    if listing.truncated {
        budget.truncated = true;
    }
    let mut entries = listing.entries.clone();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let scope = UnitScope::Project {
        name: project.to_owned(),
    };
    for entry in &entries {
        if !budget.spend(1) {
            return Ok(());
        }
        let unit_path = project_path.join(&entry.name);
        if entry.name.starts_with(state::QUARANTINE_PREFIX) {
            kept.push(kept_row(
                &entry.name,
                &scope.key(),
                Some(unit_path),
                "quarantine",
                "a pending deletion's quarantine leaf; only a replay settles it",
            ));
            continue;
        }
        if entry.facts.kind == EntryKind::Symlink {
            kept.push(kept_row(
                &entry.name,
                &scope.key(),
                Some(unit_path),
                "symlink",
                "a symbolic link is never followed or deleted",
            ));
            continue;
        }
        if entry.facts.kind == EntryKind::Other {
            kept.push(kept_row(
                &entry.name,
                &scope.key(),
                Some(unit_path),
                "other",
                "neither a file nor a directory",
            ));
            continue;
        }
        let family = if entry.facts.kind == EntryKind::Directory {
            unit_family(ctx, &project_handle, &entry.name, &unit_path)?
        } else {
            UnitFamily::Unmarked
        };
        if let Reserved::MarkedOnly(reason) = reserved {
            if family == UnitFamily::Unmarked {
                kept.push(kept_row(
                    &entry.name,
                    &scope.key(),
                    Some(unit_path),
                    "reserved",
                    reason,
                ));
                continue;
            }
        }
        classify_unit(
            ctx,
            &project_handle,
            &entry.name,
            &unit_path,
            &scope,
            family,
            budget,
            candidates,
            kept,
        )?;
    }
    Ok(())
}

/// Which recognised family identifies one unit.
fn unit_family(
    ctx: &SweepContext<'_>,
    parent: &DirHandle,
    name: &str,
    path: &Path,
) -> Result<UnitFamily> {
    let unit = match ctx.dirs.open_child_dir(parent, name) {
        Ok(unit) => unit,
        Err(_) => return Ok(UnitFamily::Unmarked),
    };
    let expectation = MarkerExpectation {
        volume_uuid: &ctx.record.volume_uuid,
        resolved_path: path,
        mount_point: ctx.record.mount_hint.as_deref(),
        effective_uid: ctx.observer.effective_uid()?,
        now_ms: ctx.now_ms,
    };
    let (marker, _) = area::read_marker_with_digest(ctx.dirs, &unit, &expectation);
    if !matches!(marker, MarkerState::Absent) {
        // A Barge marker, valid or not, makes the unit Barge's: it is never an
        // unmarked unit a legacy claim could cover.
        return Ok(UnitFamily::Native);
    }
    let oxide = read_family_marker::<OxideScratchMarker>(
        ctx.dirs,
        ctx.observer,
        &unit,
        OXIDE_MARKER_FILE,
        true,
    );
    if !matches!(oxide, MarkerReading::Absent) {
        return Ok(UnitFamily::OxideScratch);
    }
    let effigy = read_family_marker::<EffigyScratchMarker>(
        ctx.dirs,
        ctx.observer,
        &unit,
        EFFIGY_MARKER_FILE,
        false,
    );
    match effigy {
        MarkerReading::Absent => Ok(UnitFamily::Unmarked),
        _ => Ok(UnitFamily::EffigyScratch),
    }
}

/// Decide one unit and record it as a candidate or a kept row.
#[allow(clippy::too_many_arguments)]
fn classify_unit(
    ctx: &SweepContext<'_>,
    parent: &DirHandle,
    name: &str,
    path: &Path,
    scope: &UnitScope,
    family: UnitFamily,
    budget: &mut Budget,
    candidates: &mut Vec<PlanRow>,
    kept: &mut Vec<KeptRow>,
) -> Result<()> {
    if name.len() > MAX_UNIT_NAME_BYTES {
        kept.push(kept_row(
            name,
            &scope.key(),
            Some(path.to_path_buf()),
            family.as_str(),
            "the unit's name is over the state key cap, so it is never acted on",
        ));
        return Ok(());
    }
    match assess(ctx, parent, name, path, scope, family, budget)? {
        AssessmentOutcome::Eligible(assessment) => candidates.push(assessment.row),
        AssessmentOutcome::Kept { state, reason } => kept.push(kept_row(
            name,
            &scope.key(),
            Some(path.to_path_buf()),
            &state,
            &reason,
        )),
    }
    Ok(())
}

fn kept_row(name: &str, scope: &str, path: Option<PathBuf>, state: &str, reason: &str) -> KeptRow {
    KeptRow {
        name: name.to_owned(),
        scope: scope.to_owned(),
        path,
        state: state.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Assess one unit against its family's rule and its recorded grant.
fn assess(
    ctx: &SweepContext<'_>,
    parent: &DirHandle,
    name: &str,
    path: &Path,
    scope: &UnitScope,
    family: UnitFamily,
    budget: &mut Budget,
) -> Result<AssessmentOutcome> {
    let facts: EntryFacts = match ctx.dirs.child_facts(parent, name)? {
        Some(facts) => facts,
        None => {
            return Ok(AssessmentOutcome::Kept {
                state: family.as_str().to_owned(),
                reason: "the unit vanished while it was examined".to_owned(),
            })
        }
    };
    let uid = ctx.observer.effective_uid()?;
    if facts.uid != uid {
        return Ok(AssessmentOutcome::Kept {
            state: family.as_str().to_owned(),
            reason: format!("the unit is owned by uid {}, not this account's", facts.uid),
        });
    }
    if facts.device != ctx.device {
        return Ok(AssessmentOutcome::Kept {
            state: family.as_str().to_owned(),
            reason: "the unit is on another device".to_owned(),
        });
    }
    let holds = if state::is_identifier(name) {
        area::read_holds(ctx.state_paths, ctx.observer, name)?
    } else if area::validate_name("unit_name", name).is_ok() {
        area::read_unit_holds(ctx.state_paths, ctx.observer, name)?
    } else {
        area::HoldReading::empty()
    };
    if !holds.is_idle() {
        return Ok(AssessmentOutcome::Kept {
            state: family.as_str().to_owned(),
            reason: "a live or undecidable hold covers the unit".to_owned(),
        });
    }
    let outcome = if facts.kind != EntryKind::Directory {
        // Only an unmarked ad-hoc entry can be a file; a marker of any family
        // belongs to a directory.
        if family != UnitFamily::Unmarked {
            return Ok(AssessmentOutcome::Kept {
                state: family.as_str().to_owned(),
                reason: "a marked unit is a directory, not this entry".to_owned(),
            });
        }
        return Ok(
            match assess_unmarked_file(ctx, &facts, path, name, scope)? {
                Assessed::Eligible(row, observed) => {
                    AssessmentOutcome::Eligible(Box::new(Assessment {
                        row: *row,
                        observed,
                    }))
                }
                Assessed::Kept { state, reason } => AssessmentOutcome::Kept { state, reason },
            },
        );
    } else {
        let unit = match ctx.dirs.open_child_dir(parent, name) {
            Ok(unit) => unit,
            Err(error) => {
                return Ok(AssessmentOutcome::Kept {
                    state: family.as_str().to_owned(),
                    reason: format!("the unit cannot be opened: {error}"),
                })
            }
        };
        let assessment = match family {
            UnitFamily::Native => assess_native(ctx, &unit, path, name, scope),
            UnitFamily::OxideScratch => assess_oxide(ctx, &unit, path, name, scope),
            UnitFamily::EffigyScratch => assess_effigy(ctx, &unit, path, name, scope),
            UnitFamily::Unmarked => assess_unmarked(ctx, &unit, &facts, path, name, scope, budget),
        }?;
        if let Assessed::Eligible(row, _) = &assessment {
            if let Some(marker) = row.marker_file.as_deref() {
                if let Some(reason) =
                    nested_marker_in_tree(ctx.dirs, &unit, marker, 0, Path::new(""), budget)?
                {
                    return Ok(AssessmentOutcome::Kept {
                        state: family.as_str().to_owned(),
                        reason,
                    });
                }
            }
        }
        assessment
    };
    Ok(match outcome {
        Assessed::Eligible(row, observed) => AssessmentOutcome::Eligible(Box::new(Assessment {
            row: *row,
            observed,
        })),
        Assessed::Kept { state, reason } => AssessmentOutcome::Kept { state, reason },
    })
}

/// Assess one claimed unmarked entry that is a file: its newest activity is its
/// own modification time and it has no tree to check for symlinks.
fn assess_unmarked_file(
    ctx: &SweepContext<'_>,
    facts: &EntryFacts,
    path: &Path,
    name: &str,
    scope: &UnitScope,
) -> Result<Assessed> {
    let Some(claim) = ctx.claim_for(scope) else {
        return Ok(Assessed::Kept {
            state: "unmarked".to_owned(),
            reason: "no legacy claim covers this scope; keep and report what a claim would cover"
                .to_owned(),
        });
    };
    if claim.unit != scope.claim_unit() {
        return Ok(Assessed::Kept {
            state: "unmarked".to_owned(),
            reason: "the claim's declared unit shape does not match this scope".to_owned(),
        });
    }
    let newest = facts.modified_ms.unwrap_or(0);
    let idle_ms = claim.unmarked_idle_seconds.saturating_mul(1000);
    if ctx.now_ms.saturating_sub(newest) <= idle_ms {
        return Ok(Assessed::Kept {
            state: "unmarked".to_owned(),
            reason: "the entry is not older than the claim's idle age".to_owned(),
        });
    }
    let digest = unmarked_provenance_digest_parts(
        &ctx.record.volume_uuid,
        name,
        facts.device.get(),
        facts.inode,
    );
    let observed = ObservedRoot {
        device: facts.device,
        inode: facts.inode,
        marker_digest: digest.clone(),
    };
    Ok(Assessed::Eligible(
        Box::new(plan_row(
            name,
            scope,
            UnitFamily::Unmarked,
            path,
            UnitKind::File,
            facts.device.get(),
            facts.inode,
            None,
            digest,
            None,
            None,
            newest,
            "claimed unmarked entry: no marker and older than the claim's idle age",
        )),
        observed,
    ))
}

fn assess_native(
    ctx: &SweepContext<'_>,
    unit: &DirHandle,
    path: &Path,
    name: &str,
    scope: &UnitScope,
) -> Result<Assessed> {
    let expectation = MarkerExpectation {
        volume_uuid: &ctx.record.volume_uuid,
        resolved_path: path,
        mount_point: ctx.record.mount_hint.as_deref(),
        effective_uid: ctx.observer.effective_uid()?,
        now_ms: ctx.now_ms,
    };
    let (marker, digest) = area::read_marker_with_digest(ctx.dirs, unit, &expectation);
    let marker = match marker {
        MarkerState::Owned(marker) if marker.kind == RootKind::ScratchRun => marker,
        MarkerState::Owned(_) => {
            return Ok(Assessed::Kept {
                state: "native".to_owned(),
                reason: "the owned root is not a scratch run".to_owned(),
            })
        }
        MarkerState::Unknown { reason } => {
            return Ok(Assessed::Kept {
                state: "native".to_owned(),
                reason: format!("the marker is not owned: {reason}"),
            })
        }
        MarkerState::Absent => {
            return Ok(Assessed::Kept {
                state: "native".to_owned(),
                reason: "the root's marker is absent".to_owned(),
            })
        }
    };
    let Some(digest) = digest else {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason: "the marker digest could not be read".to_owned(),
        });
    };
    let Some(conclusion) = marker.conclusion else {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason: "the run is unconcluded".to_owned(),
        });
    };
    // The outcome is one of the four pinned facts, and only `closed` can carry a
    // positive closure: a retained, unclosed or failed run is never expirable,
    // whatever else its conclusion records.
    if conclusion.outcome != ConclusionOutcome::Closed {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason: format!(
                "the run's recorded outcome is `{}`, which is never expirable",
                outcome_name(conclusion.outcome)
            ),
        });
    }
    if conclusion.closure != ConclusionClosure::Positive {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason: "the run recorded `closure: none`".to_owned(),
        });
    }
    if conclusion.nested_scope != NestedScope::Exclusive {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason:
                "the run recorded a hosting scope, which only the hosting authority can conclude"
                    .to_owned(),
        });
    }
    if conclusion.kept_reason != KeptReason::Evidence {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason: format!(
                "the run recorded `kept_reason: {}`, which is not expirable",
                kept_reason_name(conclusion.kept_reason)
            ),
        });
    }
    let age_ms = ctx.policy.native_age_seconds.saturating_mul(1000);
    if ctx.now_ms.saturating_sub(conclusion.concluded_ms) <= age_ms {
        return Ok(Assessed::Kept {
            state: "native".to_owned(),
            reason: "the conclusion is not older than the policy age".to_owned(),
        });
    }
    Ok(Assessed::Eligible(
        Box::new(plan_row(
            name,
            scope,
            UnitFamily::Native,
            path,
            UnitKind::Directory,
            unit.identity().device.get(),
            unit.identity().inode,
            Some(area::ROOT_MARKER_FILE.to_owned()),
            digest.clone(),
            Some(conclusion.concluded_ms),
            Some(marker.created_ms),
            conclusion.concluded_ms,
            "native root: positive, exclusive and evidence, older than the native age",
        )),
        observed(unit, digest),
    ))
}

fn kept_reason_name(reason: KeptReason) -> &'static str {
    match reason {
        KeptReason::None => "none",
        KeptReason::Evidence => "evidence",
        KeptReason::Unclosed => "unclosed",
        KeptReason::Exception => "exception",
        KeptReason::NestedUnclosed => "nested-unclosed",
    }
}

/// The conclusion outcome's recorded name.
fn outcome_name(outcome: ConclusionOutcome) -> &'static str {
    match outcome {
        ConclusionOutcome::Closed => "closed",
        ConclusionOutcome::Unclosed => "unclosed",
        ConclusionOutcome::Failed => "failed",
        ConclusionOutcome::Retained => "retained",
    }
}

fn assess_oxide(
    ctx: &SweepContext<'_>,
    unit: &DirHandle,
    path: &Path,
    name: &str,
    scope: &UnitScope,
) -> Result<Assessed> {
    if !ctx.has_delegation(MarkerFamily::OxideScratch) {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "no delegation for the oxide_scratch family is recorded; keep and report"
                .to_owned(),
        });
    }
    let reading = read_family_marker::<OxideScratchMarker>(
        ctx.dirs,
        ctx.observer,
        unit,
        OXIDE_MARKER_FILE,
        true,
    );
    let (marker, digest) = match reading {
        MarkerReading::Present { marker, digest } => (marker, digest),
        MarkerReading::Absent => {
            return Ok(Assessed::Kept {
                state: "oxide_scratch".to_owned(),
                reason: "the oxide marker is absent".to_owned(),
            })
        }
        MarkerReading::Unknown { reason } => {
            return Ok(Assessed::Kept {
                state: "oxide_scratch".to_owned(),
                reason: format!("the oxide marker is not usable: {reason}"),
            })
        }
    };
    if marker.version != 1 {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: format!("the oxide marker's version {} is not read", marker.version),
        });
    }
    // The pinned field set's own semantics are checked before eligibility: a run
    // id that is not the 32-hex id Oxide writes, or an outcome other than
    // `closed`, is not the document this rule was written against and is kept.
    if !is_hex32(&marker.run_id) {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide run id is not a 32-hex id".to_owned(),
        });
    }
    if marker.outcome != "closed" {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: format!(
                "the oxide root's outcome `{}` is never expirable",
                marker.outcome
            ),
        });
    }
    if marker.owner_uid != ctx.observer.effective_uid()? {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide marker records another owner".to_owned(),
        });
    }
    if marker.device != unit.identity().device.get() || marker.inode != unit.identity().inode {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide marker's device and inode are not the unit's".to_owned(),
        });
    }
    if marker.root != path.display().to_string() {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide marker records another root path".to_owned(),
        });
    }
    let Some(concluded) = marker.concluded_unix else {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide root is unconcluded".to_owned(),
        });
    };
    if marker.closure != "positive" {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide root recorded a non-positive closure".to_owned(),
        });
    }
    if marker.nested_scope != "exclusive" {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide root recorded a hosting scope".to_owned(),
        });
    }
    if marker.kept_reason != "evidence" && marker.kept_reason != "qualify-base" {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: format!(
                "the oxide root's kept_reason `{}` is not expirable",
                marker.kept_reason
            ),
        });
    }
    let Some(concluded_ms) = seconds_to_ms(concluded) else {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide conclusion time is not usable".to_owned(),
        });
    };
    if concluded_ms > ctx.now_ms {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide conclusion is dated in the future".to_owned(),
        });
    }
    // A creation time that cannot be read, or that is later than the conclusion,
    // is not the sequence the family writes: it is kept rather than defaulted to
    // zero, which would silently authorise a conclusion with no known origin.
    let Some(created_ms) = seconds_to_ms(marker.created_unix) else {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide marker's creation time is not usable".to_owned(),
        });
    };
    if created_ms > ctx.now_ms {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide marker is dated in the future".to_owned(),
        });
    }
    if created_ms > concluded_ms {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide marker was created after its conclusion".to_owned(),
        });
    }
    let age_ms = ctx.policy.oxide_age_seconds.saturating_mul(1000);
    if ctx.now_ms.saturating_sub(concluded_ms) <= age_ms {
        return Ok(Assessed::Kept {
            state: "oxide_scratch".to_owned(),
            reason: "the oxide conclusion is not older than the delegated age".to_owned(),
        });
    }
    Ok(Assessed::Eligible(
        Box::new(plan_row(
            name,
            scope,
            UnitFamily::OxideScratch,
            path,
            UnitKind::Directory,
            unit.identity().device.get(),
            unit.identity().inode,
            Some(OXIDE_MARKER_FILE.to_owned()),
            digest.clone(),
            Some(concluded_ms),
            Some(created_ms),
            concluded_ms,
            "delegated oxide root: positive, exclusive and evidence, older than seven days",
        )),
        observed(unit, digest),
    ))
}

fn assess_effigy(
    ctx: &SweepContext<'_>,
    unit: &DirHandle,
    path: &Path,
    name: &str,
    scope: &UnitScope,
) -> Result<Assessed> {
    if !ctx.has_delegation(MarkerFamily::EffigyScratch) {
        return Ok(Assessed::Kept {
            state: "effigy_scratch".to_owned(),
            reason: "no delegation for the effigy_scratch family is recorded; keep and report"
                .to_owned(),
        });
    }
    let reading = read_family_marker::<EffigyScratchMarker>(
        ctx.dirs,
        ctx.observer,
        unit,
        EFFIGY_MARKER_FILE,
        false,
    );
    let (marker, digest) = match reading {
        MarkerReading::Present { marker, digest } => (marker, digest),
        MarkerReading::Absent => {
            return Ok(Assessed::Kept {
                state: "effigy_scratch".to_owned(),
                reason: "the effigy marker is absent".to_owned(),
            })
        }
        MarkerReading::Unknown { reason } => {
            return Ok(Assessed::Kept {
                state: "effigy_scratch".to_owned(),
                reason: format!("the effigy marker is not usable: {reason}"),
            })
        }
    };
    let Some(created_ms) = u64::try_from(marker.created_ms).ok() else {
        return Ok(Assessed::Kept {
            state: "effigy_scratch".to_owned(),
            reason: "the effigy marker's creation time is not usable".to_owned(),
        });
    };
    if created_ms > ctx.now_ms {
        return Ok(Assessed::Kept {
            state: "effigy_scratch".to_owned(),
            reason: "the effigy marker is dated in the future".to_owned(),
        });
    }
    // A recorded pid that is still alive keeps its directory however old it is:
    // Barge cannot prove it is a different process, so the unit is not eligible.
    if marker.pid != 0 {
        match ctx.observer.process_start(marker.pid) {
            Ok(Some(_)) => {
                return Ok(Assessed::Kept {
                    state: "effigy_scratch".to_owned(),
                    reason: format!("the effigy run's pid {} is live", marker.pid),
                })
            }
            Ok(None) => {}
            Err(error) => {
                return Ok(Assessed::Kept {
                    state: "effigy_scratch".to_owned(),
                    reason: format!(
                        "the effigy run's pid {} cannot be decided: {error}",
                        marker.pid
                    ),
                })
            }
        }
    }
    let concluded = marker
        .concluded_ms
        .and_then(|value| u64::try_from(value).ok());
    let (eligible, order_ms, detail) = match concluded {
        Some(concluded) => {
            if concluded > ctx.now_ms {
                return Ok(Assessed::Kept {
                    state: "effigy_scratch".to_owned(),
                    reason: "the effigy conclusion is dated in the future".to_owned(),
                });
            }
            let keep = marker.keep == Some(true);
            let age_ms = ctx.policy.effigy_keep_age_seconds.saturating_mul(1000);
            (
                keep && ctx.now_ms.saturating_sub(concluded) > age_ms,
                concluded,
                if keep {
                    "delegated effigy run: concluded and kept, older than its retention"
                } else {
                    "delegated effigy run: concluded without a keep request"
                },
            )
        }
        None => {
            let gone = marker.pid != 0;
            let age_ms = ctx.policy.effigy_age_seconds.saturating_mul(1000);
            let old = ctx.now_ms.saturating_sub(created_ms) > age_ms;
            (
                gone || old,
                created_ms,
                if marker.pid == 0 {
                    "delegated effigy run: unconcluded, liveness undecidable, older than seven days"
                } else {
                    "delegated effigy run: unconcluded and its process is positively gone"
                },
            )
        }
    };
    if !eligible {
        return Ok(Assessed::Kept {
            state: "effigy_scratch".to_owned(),
            reason: detail.to_owned(),
        });
    }
    Ok(Assessed::Eligible(
        Box::new(plan_row(
            name,
            scope,
            UnitFamily::EffigyScratch,
            path,
            UnitKind::Directory,
            unit.identity().device.get(),
            unit.identity().inode,
            Some(EFFIGY_MARKER_FILE.to_owned()),
            digest.clone(),
            concluded,
            Some(created_ms),
            order_ms,
            detail,
        )),
        observed(unit, digest),
    ))
}

#[allow(clippy::too_many_arguments)]
fn assess_unmarked(
    ctx: &SweepContext<'_>,
    unit: &DirHandle,
    facts: &EntryFacts,
    path: &Path,
    name: &str,
    scope: &UnitScope,
    budget: &mut Budget,
) -> Result<Assessed> {
    let Some(claim) = ctx.claim_for(scope) else {
        return Ok(Assessed::Kept {
            state: "unmarked".to_owned(),
            reason: "no legacy claim covers this scope; keep and report what a claim would cover"
                .to_owned(),
        });
    };
    if claim.unit != scope.claim_unit() {
        return Ok(Assessed::Kept {
            state: "unmarked".to_owned(),
            reason: "the claim's declared unit shape does not match this scope".to_owned(),
        });
    }
    let newest = match newest_entry(
        ctx.dirs,
        unit,
        facts.modified_ms.unwrap_or(0),
        0,
        Path::new(""),
        budget,
    )? {
        Ok(newest) => newest,
        Err(reason) => {
            return Ok(Assessed::Kept {
                state: "unmarked".to_owned(),
                reason,
            })
        }
    };
    let idle_ms = claim.unmarked_idle_seconds.saturating_mul(1000);
    if ctx.now_ms.saturating_sub(newest) <= idle_ms {
        return Ok(Assessed::Kept {
            state: "unmarked".to_owned(),
            reason: "the unit's newest entry is not older than the claim's idle age".to_owned(),
        });
    }
    let identity = unit.identity();
    let digest = unmarked_provenance_digest_parts(
        &ctx.record.volume_uuid,
        name,
        identity.device.get(),
        identity.inode,
    );
    Ok(Assessed::Eligible(
        Box::new(plan_row(
            name,
            scope,
            UnitFamily::Unmarked,
            path,
            UnitKind::Directory,
            identity.device.get(),
            identity.inode,
            None,
            digest.clone(),
            None,
            None,
            newest,
            "claimed unmarked unit: no marker, no symlink inside and older than the claim's idle age",
        )),
        observed(unit, digest),
    ))
}

/// Refuse a marked candidate whose bounded tree contains another recognised
/// marker. The unit's own marker is the sole top-level exception; links are
/// never followed, and S5 still decides whether other entries are removable.
fn nested_marker_in_tree(
    dirs: &dyn DirOps,
    handle: &DirHandle,
    own_marker: &str,
    depth: usize,
    relative: &Path,
    budget: &mut Budget,
) -> Result<Option<String>> {
    if depth > delete::DELETE_DEPTH || !budget.spend(1) {
        return Ok(Some(
            "the bounded marker walk could not complete".to_owned(),
        ));
    }
    let listing = match dirs.list_children(handle, LISTING_NAME_ROWS) {
        Ok(listing) if !listing.truncated => listing,
        Ok(_) => {
            return Ok(Some(
                "the marker walk reached the directory listing bound".to_owned(),
            ))
        }
        Err(error) => {
            return Ok(Some(format!(
                "the marker walk cannot list the unit: {error}"
            )))
        }
    };
    for entry in &listing.entries {
        if !budget.spend(1) {
            return Ok(Some("the marker walk reached the sweep budget".to_owned()));
        }
        let path = relative.join(&entry.name);
        if crate::unit::MarkerFamily::from_marker_file(&entry.name).is_some()
            && !(depth == 0 && entry.name == own_marker)
        {
            return Ok(Some(format!(
                "recognised marker `{}` inside the unit is not covered by its grant",
                path.display()
            )));
        }
        if entry.facts.kind == EntryKind::Directory {
            let child = match dirs.open_child_dir(handle, &entry.name) {
                Ok(child) => child,
                Err(error) => {
                    return Ok(Some(format!(
                        "the marker walk cannot open `{}`: {error}",
                        path.display()
                    )))
                }
            };
            let identity = child.identity();
            if identity.device != entry.facts.device || identity.inode != entry.facts.inode {
                return Ok(Some(format!(
                    "`{}` changed during the marker walk",
                    path.display()
                )));
            }
            if let Some(reason) =
                nested_marker_in_tree(dirs, &child, own_marker, depth + 1, &path, budget)?
            {
                return Ok(Some(reason));
            }
        }
    }
    Ok(None)
}

/// The newest modification time in one claimed unit's tree, refusing a symlink
/// or a recognised marker at any level and bounded by the sweep budget.
fn newest_entry(
    dirs: &dyn DirOps,
    handle: &DirHandle,
    own_modified_ms: u64,
    depth: usize,
    relative: &Path,
    budget: &mut Budget,
) -> Result<std::result::Result<u64, String>> {
    if depth > delete::DELETE_DEPTH {
        return Ok(Err(format!(
            "the unit is deeper than the {} level bound",
            delete::DELETE_DEPTH
        )));
    }
    if !budget.spend(1) {
        return Ok(Err(
            "the sweep's examination budget was reached before this unit".to_owned(),
        ));
    }
    let listing = match dirs.list_children(handle, LISTING_NAME_ROWS) {
        Ok(listing) => listing,
        Err(error) => return Ok(Err(format!("the unit cannot be listed: {error}"))),
    };
    if listing.truncated {
        return Ok(Err(
            "the unit holds more names than the listing bound".to_owned()
        ));
    }
    let mut newest = own_modified_ms;
    for entry in &listing.entries {
        // Every examined entry is charged to the sweep's budget and checked
        // against its deadline: a unit holding many files must not be walked
        // past the bound the plan claims it applied.
        if !budget.spend(1) {
            return Ok(Err(
                "the sweep's examination budget was reached inside this unit".to_owned(),
            ));
        }
        if crate::unit::MarkerFamily::from_marker_file(&entry.name).is_some() {
            return Ok(Err(format!(
                "recognised marker `{}` inside the claimed unit is not covered by its claim",
                relative.join(&entry.name).display()
            )));
        }
        if entry.facts.kind == EntryKind::Symlink {
            return Ok(Err(format!(
                "`{}` is a symbolic link inside the unit, which is never followed",
                entry.name
            )));
        }
        newest = newest.max(entry.facts.modified_ms.unwrap_or(0));
        if entry.facts.kind == EntryKind::Directory {
            let child = match dirs.open_child_dir(handle, &entry.name) {
                Ok(child) => child,
                Err(error) => return Ok(Err(format!("the unit cannot be walked: {error}"))),
            };
            match newest_entry(
                dirs,
                &child,
                0,
                depth + 1,
                &relative.join(&entry.name),
                budget,
            )? {
                Ok(inner) => newest = newest.max(inner),
                Err(reason) => return Ok(Err(reason)),
            }
        }
    }
    Ok(Ok(newest))
}

#[allow(clippy::too_many_arguments)]
fn plan_row(
    name: &str,
    scope: &UnitScope,
    family: UnitFamily,
    path: &Path,
    kind: UnitKind,
    device: u64,
    inode: u64,
    marker_file: Option<String>,
    marker_digest: String,
    concluded_ms: Option<u64>,
    created_ms: Option<u64>,
    last_activity_ms: u64,
    reason: &str,
) -> PlanRow {
    PlanRow {
        name: name.to_owned(),
        scope: scope.clone(),
        family,
        path: path.to_path_buf(),
        kind,
        device,
        inode,
        marker_file,
        marker_digest,
        concluded_ms,
        created_ms,
        last_activity_ms,
        grant: family.grant().to_owned(),
        reason: reason.to_owned(),
    }
}

fn observed(unit: &DirHandle, marker_digest: String) -> ObservedRoot {
    ObservedRoot {
        device: unit.identity().device,
        inode: unit.identity().inode,
        marker_digest,
    }
}

/// Re-assess one planned unit immediately before its effect.
#[allow(clippy::too_many_arguments)]
fn reassess(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    volume_uuid: &VolumeUuid,
    scope: &UnitScope,
    name: &str,
    now_ms: u64,
) -> Result<Option<Assessment>> {
    let record = area::registered_volume(state_paths, volume_uuid)?;
    // A record that no longer permits pruning keeps this unit, as a per-unit
    // refusal: an `undeclared` record is loud at the volume level, and a `kept`
    // one is quiet, but neither is a reason to stop the whole stage.
    if !record.durability.permits_pruning() {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "volume {} does not permit pruning now (`{}`), so the unit is kept",
                record.volume_uuid,
                durability_name(record.durability)
            ),
        )
        .with_detail("volume_uuid", record.volume_uuid.as_str().into()));
    }
    record.declared_durability()?;
    reassess_under(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        &record,
        scope,
        name,
        now_ms,
    )
}

/// Re-assess one planned unit against a registry record already in hand.
///
/// The deletion's locked window passes the record it re-read there, so the
/// eligibility it checks is the current policy, never the plan's older reading.
#[allow(clippy::too_many_arguments)]
fn reassess_under(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    record: &VolumeRecord,
    scope: &UnitScope,
    name: &str,
    now_ms: u64,
) -> Result<Option<Assessment>> {
    let scratch = scratch_folder(record);
    let folder = scope.parent_folder(&scratch);
    let Some((path, handle)) = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &record.root_dir,
        &folder,
    )?
    else {
        return Ok(None);
    };
    match dirs.child_facts(&handle, name)? {
        Some(facts) if matches!(facts.kind, EntryKind::Directory | EntryKind::File) => {}
        _ => return Ok(None),
    }
    let unit_path = path.join(name);
    let ctx = SweepContext {
        dirs,
        observer,
        state_paths,
        record,
        device: handle.identity().device,
        now_ms,
        grants: &record.scratch_grants,
        policy: SweepPolicy::design(),
    };
    let family = unit_family(&ctx, &handle, name, &unit_path)?;
    let mut budget = Budget::new(SweepBounds::default());
    match assess(&ctx, &handle, name, &unit_path, scope, family, &mut budget)? {
        AssessmentOutcome::Eligible(assessment) => Ok(Some(*assessment)),
        AssessmentOutcome::Kept { .. } => Ok(None),
    }
}

/// Unix seconds to milliseconds, refusing an unusable value.
fn seconds_to_ms(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let ms = seconds * 1000.0;
    if ms >= u64::MAX as f64 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = ms as u64;
    Some(value)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether a value is the 32-character lowercase hexadecimal id a run records.
fn is_hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::{
        alloc, conclude, declare_claim, declare_delegation, expire_plan, plan_digest,
        scratch_folder, sweep, unmarked_provenance_digest_parts, AllocRequest, ConcludeRequest,
        MarkerReading, SweepBounds, SweepPolicy, SweepRequest, UnitFamily, EFFIGY_AGE_SECONDS,
        EFFIGY_KEEP_AGE_SECONDS, NATIVE_AGE_SECONDS, OXIDE_AGE_SECONDS,
    };
    use crate::area::{
        self, ConclusionClosure, ConclusionOutcome, HoldRequest, HoldScope, KeptReason, NestedScope,
    };
    use crate::contract::MARKER_CAP_BYTES;
    use crate::delete::DeleteBounds;
    use crate::error::ErrorKind;
    use crate::os::{DirHandle, DirOps, ProcessObserver};
    use crate::registry::{ClaimScope, ClaimUnit, MarkerFamily, ScratchClaim, ScratchDelegation};
    use crate::state::{self, IntentPhase};
    use crate::testkit::{Faults, VolumeFixture, FIXTURE_UUID};

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    /// A sweep "now" that is `days` days and `extra_ms` past the real clock, so a
    /// root concluded just now is exactly as old as the test needs.
    fn sweep_now(days: u64, extra_ms: u64) -> u64 {
        state::now_ms() + days * DAY_MS + extra_ms
    }

    struct Harness {
        fixture: VolumeFixture,
    }

    impl Harness {
        fn new(label: &str) -> Self {
            let fixture = VolumeFixture::new(label);
            fixture.register();
            Self { fixture }
        }

        fn scratch(&self) -> PathBuf {
            self.fixture.root.join(scratch_folder(&self.fixture.record))
        }

        fn project(&self, project: &str) -> PathBuf {
            let path = self.scratch().join(project);
            std::fs::create_dir_all(&path).expect("the project folder");
            path
        }

        fn alloc(&self, project: &str, consumer: &str) -> PathBuf {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let platform = self.fixture.platform();
            let report = alloc(
                &self.fixture.state,
                &platform,
                &facts,
                &mounts,
                &observer,
                &AllocRequest {
                    volume_uuid: self.fixture.uuid.clone(),
                    project: project.to_owned(),
                    consumer: consumer.to_owned(),
                    pid: std::process::id(),
                },
            )
            .expect("the scratch root is allocated");
            report.root.path
        }

        fn conclude(
            &self,
            path: &Path,
            holder: &str,
            outcome: ConclusionOutcome,
            closure: ConclusionClosure,
            kept_reason: KeptReason,
            nested_scope: NestedScope,
        ) {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let platform = self.fixture.platform();
            let root_id = path
                .file_name()
                .expect("a root name")
                .to_string_lossy()
                .to_string();
            conclude(
                &self.fixture.state,
                &platform,
                &facts,
                &mounts,
                &observer,
                &ConcludeRequest {
                    volume_uuid: &self.fixture.uuid,
                    root_id: &root_id,
                    holder,
                    project: None,
                    outcome,
                    closure,
                    kept_reason,
                    nested_scope,
                },
            )
            .expect("the run concludes");
            // A concluded run releases its owner hold; that is what makes the
            // root idle, and expiry is never taken on a live holder's root.
            area::release(
                &self.fixture.state,
                &self.fixture.observer(),
                &root_id,
                holder,
            )
            .expect("the hold is released");
        }

        fn plan(&self, days: u64) -> super::ExpireReport {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let platform = self.fixture.platform();
            let uuid = self.fixture.uuid.clone();
            let request = SweepRequest {
                volume_uuid: &uuid,
                preview: true,
                plan_digest: None,
                bounds: SweepBounds::default(),
                delete_bounds: DeleteBounds {
                    retry_pause: std::time::Duration::ZERO,
                    ..DeleteBounds::default()
                },
                now_ms: sweep_now(days, 0),
            };
            sweep(
                &self.fixture.state,
                &platform,
                &facts,
                &mounts,
                &observer,
                &request,
            )
            .expect("the plan is computed")
            .report
        }

        fn plan_at(&self, now_ms: u64) -> super::ExpireReport {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let platform = self.fixture.platform();
            let uuid = self.fixture.uuid.clone();
            let request = SweepRequest {
                volume_uuid: &uuid,
                preview: true,
                plan_digest: None,
                bounds: SweepBounds::default(),
                delete_bounds: DeleteBounds {
                    retry_pause: std::time::Duration::ZERO,
                    ..DeleteBounds::default()
                },
                now_ms,
            };
            sweep(
                &self.fixture.state,
                &platform,
                &facts,
                &mounts,
                &observer,
                &request,
            )
            .expect("the plan is computed")
            .report
        }

        fn expire_at(&self, now_ms: u64, digest: &str) -> super::ExpireReport {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let platform = self.fixture.platform();
            let uuid = self.fixture.uuid.clone();
            let request = SweepRequest {
                volume_uuid: &uuid,
                preview: false,
                plan_digest: Some(digest),
                bounds: SweepBounds::default(),
                delete_bounds: DeleteBounds {
                    retry_pause: std::time::Duration::ZERO,
                    ..DeleteBounds::default()
                },
                now_ms,
            };
            sweep(
                &self.fixture.state,
                &platform,
                &facts,
                &mounts,
                &observer,
                &request,
            )
            .expect("the real sweep reports")
            .report
        }

        fn expire(&self, days: u64, digest: &str) -> super::ExpireReport {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let platform = self.fixture.platform();
            let uuid = self.fixture.uuid.clone();
            let request = SweepRequest {
                volume_uuid: &uuid,
                preview: false,
                plan_digest: Some(digest),
                bounds: SweepBounds::default(),
                delete_bounds: DeleteBounds {
                    retry_pause: std::time::Duration::ZERO,
                    ..DeleteBounds::default()
                },
                now_ms: sweep_now(days, 0),
            };
            sweep(
                &self.fixture.state,
                &platform,
                &facts,
                &mounts,
                &observer,
                &request,
            )
            .expect("the real sweep reports")
            .report
        }

        fn expire_with(&self, dirs: &dyn DirOps, days: u64, digest: &str) -> super::ExpireReport {
            let observer = self.fixture.observer();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let uuid = self.fixture.uuid.clone();
            let request = SweepRequest {
                volume_uuid: &uuid,
                preview: false,
                plan_digest: Some(digest),
                bounds: SweepBounds::default(),
                delete_bounds: DeleteBounds {
                    retry_pause: std::time::Duration::ZERO,
                    ..DeleteBounds::default()
                },
                now_ms: sweep_now(days, 0),
            };
            sweep(
                &self.fixture.state,
                dirs,
                &facts,
                &mounts,
                &observer,
                &request,
            )
            .expect("the real sweep reports")
            .report
        }

        fn claim(&self, project: &str, idle: u64) {
            let claim = ScratchClaim {
                scope: ClaimScope::Project {
                    name: project.to_owned(),
                },
                unit: ClaimUnit::Child,
                unmarked_idle_seconds: idle,
                families_excluded: vec![MarkerFamily::OxideScratch, MarkerFamily::EffigyScratch],
                declared_by: "operator".to_owned(),
                declared_ms: state::now_ms(),
            };
            declare_claim(&self.fixture.state, &self.fixture.uuid, &claim, false)
                .expect("the claim is recorded");
        }

        fn adhoc_claim(&self, idle: u64) {
            let claim = ScratchClaim {
                scope: ClaimScope::TopLevelAdhoc,
                unit: ClaimUnit::Entry,
                unmarked_idle_seconds: idle,
                families_excluded: vec![MarkerFamily::OxideScratch, MarkerFamily::EffigyScratch],
                declared_by: "operator".to_owned(),
                declared_ms: state::now_ms(),
            };
            declare_claim(&self.fixture.state, &self.fixture.uuid, &claim, false)
                .expect("the claim is recorded");
        }

        fn delegate(&self, family: MarkerFamily) {
            declare_delegation(
                &self.fixture.state,
                &self.fixture.uuid,
                family,
                "family owner",
                state::now_ms(),
                false,
            )
            .expect("the delegation is recorded");
        }

        fn candidate_names(&self, report: &super::ExpireReport) -> Vec<String> {
            report
                .plan
                .candidates
                .rows
                .iter()
                .map(|row| row.name.clone())
                .collect()
        }

        fn kept_reason(&self, report: &super::ExpireReport, name: &str) -> String {
            report
                .plan
                .kept
                .rows
                .iter()
                .find(|row| row.name == name)
                .map(|row| row.reason.clone())
                .unwrap_or_else(|| panic!("`{name}` is not reported as kept"))
        }
    }

    /// Write a marker file privately, as its owning family would.
    fn write_marker(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("the marker is written");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("the marker is private");
    }

    /// The device and inode of one path, as the platform reports them.
    fn identity(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).expect("the path exists");
        (metadata.dev(), metadata.ino())
    }

    fn oxide_marker(path: &Path, nested_scope: &str, kept_reason: &str, concluded: Option<u64>) {
        let (device, inode) = identity(path);
        let value = serde_json::json!({
            "version": 1,
            "run_id": "0123456789abcdef0123456789abcdef",
            "root": path.display().to_string(),
            "owner_uid": crate::os::platform().effective_uid().expect("the uid"),
            "device": device,
            "inode": inode,
            "created_unix": (concluded.unwrap_or(state::now_ms()) as f64) / 1000.0 - 60.0,
            "concluded_unix": concluded.map(|ms| ms as f64 / 1000.0),
            "outcome": "closed",
            "closure": "positive",
            "kept_reason": kept_reason,
            "nested_scope": nested_scope,
        });
        write_marker(
            &path.join(super::OXIDE_MARKER_FILE),
            &serde_json::to_vec(&value).unwrap(),
        );
    }

    fn effigy_marker(path: &Path, pid: u32, keep: bool, concluded_ms: Option<u64>) {
        let value = serde_json::json!({
            "run": "abcdefgh",
            "task": "a task",
            "checkout": "/checkout",
            "pid": pid,
            "pid_start": "1:2",
            "created_ms": state::now_ms() as i64 - 90 * 60 * 1000,
            "keep": if keep { Some(true) } else { None },
            "concluded_ms": concluded_ms.map(|ms| ms as i64),
        });
        write_marker(
            &path.join(super::EFFIGY_MARKER_FILE),
            &serde_json::to_vec(&value).unwrap(),
        );
    }

    // ---- allocation --------------------------------------------------------

    #[test]
    fn two_concurrent_allocations_get_distinct_ids() {
        let harness = Harness::new("scratch-concurrent");
        harness.project("shared");
        let uuid = harness.fixture.uuid.clone();
        let state_paths = harness.fixture.state.clone();
        let root = harness.fixture.root.clone();
        let device = harness.fixture.device;
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for consumer in ["first", "second"] {
                let state_paths = state_paths.clone();
                let uuid = uuid.clone();
                let root = root.clone();
                handles.push(scope.spawn(move || {
                    let facts = crate::testkit::FakeVolume {
                        root: root.clone(),
                        uuid: uuid.clone(),
                        device,
                        present: std::cell::Cell::new(true),
                        shifted: std::cell::Cell::new(false),
                    };
                    let mounts = crate::testkit::FakeMounts { root: root.clone() };
                    let observer = crate::testkit::FakeObserver::new();
                    let platform = crate::os::platform();
                    alloc(
                        &state_paths,
                        &platform,
                        &facts,
                        &mounts,
                        &observer,
                        &AllocRequest {
                            volume_uuid: uuid,
                            project: "shared".to_owned(),
                            consumer: consumer.to_owned(),
                            pid: std::process::id(),
                        },
                    )
                    .expect("each allocation succeeds")
                    .root
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("the thread reports"))
                .collect::<Vec<_>>()
        });
        assert_eq!(results.len(), 2);
        assert_ne!(results[0].root_id, results[1].root_id);
        for root in &results {
            assert!(root.path.is_dir(), "each root exists");
            let mode = std::fs::metadata(&root.path)
                .expect("the root's facts")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "a scratch root is private");
        }
    }

    #[test]
    fn allocation_creates_the_project_folder_as_a_plain_directory() {
        let harness = Harness::new("scratch-project-mode");
        harness.alloc("project-a", "consumer");
        let mode = std::fs::metadata(harness.scratch().join("project-a"))
            .expect("the project folder")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "a project folder is a plain directory");
    }

    // ---- conclusion --------------------------------------------------------

    #[test]
    fn a_conclusion_requires_the_live_owner_hold() {
        let harness = Harness::new("scratch-conclude-owner");
        let path = harness.alloc("project-a", "owner");
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let root_id = path.file_name().unwrap().to_string_lossy().to_string();
        let error = conclude(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &ConcludeRequest {
                volume_uuid: &harness.fixture.uuid,
                root_id: &root_id,
                holder: "somebody-else",
                project: None,
                outcome: ConclusionOutcome::Closed,
                closure: ConclusionClosure::Positive,
                kept_reason: KeptReason::Evidence,
                nested_scope: NestedScope::Exclusive,
            },
        )
        .expect_err("a stranger is not the recorded owner");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
    }

    #[test]
    fn a_conclusion_is_terminal() {
        let harness = Harness::new("scratch-conclude-terminal");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let root_id = path.file_name().unwrap().to_string_lossy().to_string();
        let error = conclude(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &ConcludeRequest {
                volume_uuid: &harness.fixture.uuid,
                root_id: &root_id,
                holder: "owner",
                project: None,
                outcome: ConclusionOutcome::Retained,
                closure: ConclusionClosure::None,
                kept_reason: KeptReason::Exception,
                nested_scope: NestedScope::Exclusive,
            },
        )
        .expect_err("a conclusion is never downgraded");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn positive_closure_needs_no_other_live_hold() {
        let harness = Harness::new("scratch-conclude-evidence");
        let path = harness.alloc("project-a", "owner");
        let root_id = path.file_name().unwrap().to_string_lossy().to_string();
        let observer = harness.fixture.observer();
        area::hold(
            &harness.fixture.state,
            &observer,
            &HoldRequest {
                root_id: root_id.clone(),
                holder: "reader".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Read,
                purpose: "a fixture reader".to_owned(),
            },
        )
        .expect("the second hold is recorded");

        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let error = conclude(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &ConcludeRequest {
                volume_uuid: &harness.fixture.uuid,
                root_id: &root_id,
                holder: "owner",
                project: None,
                outcome: ConclusionOutcome::Closed,
                closure: ConclusionClosure::Positive,
                kept_reason: KeptReason::Evidence,
                nested_scope: NestedScope::Exclusive,
            },
        )
        .expect_err("another live hold means the closure is not supported");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);

        // The same root concludes honestly without a positive closure claim.
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Unclosed,
            ConclusionClosure::None,
            KeptReason::Unclosed,
            NestedScope::Exclusive,
        );
    }

    #[test]
    fn a_conclusion_bumps_the_generation_and_keeps_the_identity() {
        let harness = Harness::new("scratch-conclude-marker");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let bytes = std::fs::read(path.join(area::ROOT_MARKER_FILE)).expect("the marker");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("the marker parses");
        assert_eq!(value["generation"], 2);
        assert_eq!(value["conclusion"]["closure"], "positive");
        assert_eq!(value["conclusion"]["nested_scope"], "exclusive");
        assert_eq!(value["kind"], "scratch_run");
    }

    // ---- expiry ------------------------------------------------------------

    #[test]
    fn a_positively_concluded_root_expires_and_an_unconcluded_one_survives() {
        let harness = Harness::new("scratch-expire-native");
        let concluded = harness.alloc("project-a", "owner-a");
        let unconcluded = harness.alloc("project-a", "owner-b");
        harness.conclude(
            &concluded,
            "owner-a",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        std::fs::write(concluded.join("payload.txt"), b"bytes").expect("a payload");
        std::fs::write(unconcluded.join("payload.txt"), b"bytes").expect("a payload");

        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        assert_eq!(
            harness.candidate_names(&plan),
            vec![concluded.file_name().unwrap().to_string_lossy().to_string()],
            "only the positively concluded root is a candidate"
        );
        let unconcluded_reason =
            harness.kept_reason(&plan, &unconcluded.file_name().unwrap().to_string_lossy());
        assert!(!unconcluded_reason.is_empty(), "{:?}", plan.plan.kept.rows);

        let report = harness.expire(days, &plan.plan_digest);
        assert!(!report.has_pending(), "{:?}", report.pending.rows);
        assert_eq!(report.deleted.count, 1);
        assert!(
            report.deleted.rows[0].marker_removed,
            "the marker goes last"
        );
        assert!(!concluded.exists(), "the concluded root is gone");
        assert!(unconcluded.is_dir(), "the unconcluded root survives");
    }

    #[test]
    fn closure_none_exception_and_hosting_are_never_eligible() {
        let harness = Harness::new("scratch-expire-ineligible");
        let none = harness.alloc("project-a", "a");
        harness.conclude(
            &none,
            "a",
            ConclusionOutcome::Unclosed,
            ConclusionClosure::None,
            KeptReason::Unclosed,
            NestedScope::Exclusive,
        );
        let exception = harness.alloc("project-a", "b");
        harness.conclude(
            &exception,
            "b",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Exception,
            NestedScope::Exclusive,
        );
        let hosting = harness.alloc("project-a", "c");
        harness.conclude(
            &hosting,
            "c",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Hosting,
        );
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        assert!(
            plan.plan.candidates.rows.is_empty(),
            "{:?}",
            plan.plan.candidates
        );
        for path in [&none, &exception, &hosting] {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let reason = harness.kept_reason(&plan, &name);
            assert!(!reason.is_empty(), "{name} is reported with a reason");
        }
    }

    #[test]
    fn a_future_dated_conclusion_is_not_eligible() {
        let harness = Harness::new("scratch-expire-future");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let marker = path.join(area::ROOT_MARKER_FILE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        value["conclusion"]["concluded_ms"] = serde_json::json!(state::now_ms() + DAY_MS * 30);
        write_marker(&marker, &serde_json::to_vec(&value).unwrap());

        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        assert!(
            plan.plan.candidates.rows.is_empty(),
            "{:?}",
            plan.plan.candidates
        );
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(harness.kept_reason(&plan, &name).contains("future"));
    }

    #[test]
    fn a_live_hold_keeps_an_eligible_root() {
        let harness = Harness::new("scratch-expire-hold");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let observer = harness.fixture.observer();
        area::hold(
            &harness.fixture.state,
            &observer,
            &HoldRequest {
                root_id: path.file_name().unwrap().to_string_lossy().to_string(),
                holder: "reader".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Read,
                purpose: "busy".to_owned(),
            },
        )
        .expect("the hold is recorded");
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        assert!(plan.plan.candidates.rows.is_empty());
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(harness.kept_reason(&plan, &name).contains("hold"));
    }

    #[test]
    fn a_plan_digest_is_required_and_must_match() {
        let harness = Harness::new("scratch-expire-digest");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let request = SweepRequest {
            volume_uuid: &harness.fixture.uuid,
            preview: false,
            plan_digest: None,
            bounds: SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            now_ms: sweep_now(days, 0),
        };
        let error = sweep(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect_err("a real run without a digest refuses");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(path.is_dir(), "nothing was deleted");

        let plan = harness.plan(days);
        let wrong = "ab".repeat(32);
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let request = SweepRequest {
            volume_uuid: &harness.fixture.uuid,
            preview: false,
            plan_digest: Some(&wrong),
            bounds: SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            now_ms: sweep_now(days, 0),
        };
        let error = sweep(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect_err("a different digest refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(path.is_dir(), "nothing was deleted");
        let expected = plan_digest(&plan.plan).expect("the plan digest");
        assert_eq!(plan.plan_digest, expected);
        let mut changed_kind = plan.plan.clone();
        changed_kind.candidates.rows[0].kind = crate::unit::UnitKind::File;
        assert_ne!(
            plan_digest(&changed_kind).expect("changed plan digest"),
            expected
        );
        assert!(harness.expire(days, &expected).deleted.count == 1);
    }

    #[test]
    fn a_marker_that_changes_between_plan_and_effect_stops_only_that_unit() {
        let harness = Harness::new("scratch-expire-race");
        let first = harness.alloc("project-a", "a");
        let second = harness.alloc("project-a", "b");
        for (path, holder) in [(&first, "a"), (&second, "b")] {
            harness.conclude(
                path,
                holder,
                ConclusionOutcome::Closed,
                ConclusionClosure::Positive,
                KeptReason::Evidence,
                NestedScope::Exclusive,
            );
        }
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        assert_eq!(plan.plan.candidates.count, 2);

        // The second root's marker is rewritten the moment the real run reads it
        // for its own plan: the plan still matches the preview, and the effect's
        // re-verification sees a different identity, so only that unit is
        // abandoned.
        let racing = RaceOnce::new(second.join(area::ROOT_MARKER_FILE));
        let report = harness.expire_with(&racing, days, &plan.plan_digest);
        assert_eq!(report.deleted.count, 1, "{:?}", report.skipped.rows);
        assert_eq!(report.skipped.count, 1, "{:?}", report.deleted.rows);
        assert!(report.skipped.rows[0].reason.contains("identity"));
        assert!(
            !first.exists(),
            "the unaffected root is deleted: {report:?}"
        );
        assert!(second.exists(), "the racing root survives: {report:?}");
    }

    #[test]
    fn a_revoked_claim_returns_the_scope_to_keep_and_report() {
        let harness = Harness::new("scratch-expire-revoked");
        let unit = harness.project("alpha").join("leftover");
        std::fs::create_dir_all(&unit).expect("the unit");
        harness.claim("alpha", 60);
        let days = 1;
        let first = harness.plan(days);
        assert_eq!(first.plan.candidates.count, 1);

        let claim = ScratchClaim {
            scope: ClaimScope::Project {
                name: "alpha".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 60,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        declare_claim(&harness.fixture.state, &harness.fixture.uuid, &claim, true)
            .expect("the claim is withdrawn");
        let second = harness.plan(days);
        assert_eq!(second.plan.candidates.count, 0);
        assert!(harness
            .kept_reason(&second, "leftover")
            .contains("no legacy claim"));
        assert!(unit.is_dir(), "the revoked claim deletes nothing");
    }

    // ---- foreign families and legacy claims --------------------------------

    #[test]
    fn a_foreign_family_and_an_unmarked_unit_survive_without_grants() {
        let harness = Harness::new("scratch-foreign-kept");
        let oxide = harness.project("oxide").join("oxide-proof.run1");
        std::fs::create_dir_all(&oxide).expect("the oxide root");
        oxide_marker(
            &oxide,
            "exclusive",
            "evidence",
            Some(state::now_ms() - 30 * DAY_MS),
        );
        let effigy = harness.scratch().join("effigy-run");
        std::fs::create_dir_all(&effigy).expect("the effigy run");
        effigy_marker(
            &effigy,
            0xFFFF_FFFF,
            true,
            Some(state::now_ms() - 30 * DAY_MS),
        );
        let unmarked = harness.project("alpha").join("leftover");
        std::fs::create_dir_all(&unmarked).expect("the unmarked unit");
        let validation = harness
            .scratch()
            .join(super::OXIDE_VALIDATION_DIR)
            .join("run.1");
        std::fs::create_dir_all(&validation).expect("the validation temp");

        let days = 8;
        let plan = harness.plan(days);
        assert!(
            plan.plan.candidates.rows.is_empty(),
            "{:?}",
            plan.plan.candidates
        );
        let reasons: Vec<String> = plan
            .plan
            .kept
            .rows
            .iter()
            .map(|row| format!("{}: {}", row.name, row.reason))
            .collect();
        for kept in [&oxide, &effigy, &unmarked] {
            assert!(
                reasons
                    .iter()
                    .any(|line| line.contains(kept.file_name().unwrap().to_string_lossy().as_ref())),
                "{} is reported kept in {reasons:?}",
                kept.display()
            );
        }
        let validation_reason = harness.kept_reason(&plan, super::OXIDE_VALIDATION_DIR);
        assert!(
            validation_reason.contains("validation temp"),
            "{validation_reason}"
        );
        assert!(oxide.exists() && effigy.exists() && unmarked.exists() && validation.exists());
    }

    #[test]
    fn with_grants_only_the_eligible_foreign_and_unmarked_units_go() {
        let harness = Harness::new("scratch-foreign-granted");
        let eligible_oxide = harness.project("oxide").join("oxide-proof.old");
        std::fs::create_dir_all(&eligible_oxide).expect("the oxide root");
        oxide_marker(
            &eligible_oxide,
            "exclusive",
            "evidence",
            Some(state::now_ms() - 30 * DAY_MS),
        );
        let hosting_oxide = harness.project("oxide").join("oxide-proof.hosting");
        std::fs::create_dir_all(&hosting_oxide).expect("the hosting root");
        oxide_marker(
            &hosting_oxide,
            "hosting",
            "evidence",
            Some(state::now_ms() - 30 * DAY_MS),
        );
        let kept_effigy = harness.scratch().join("live-run");
        std::fs::create_dir_all(&kept_effigy).expect("the effigy run");
        effigy_marker(
            &kept_effigy,
            std::process::id(),
            true,
            Some(state::now_ms() - 30 * DAY_MS),
        );
        let eligible_effigy = harness.scratch().join("dead-run");
        std::fs::create_dir_all(&eligible_effigy).expect("the effigy run");
        effigy_marker(
            &eligible_effigy,
            0xFFFF_FFFE,
            true,
            Some(state::now_ms() - 30 * DAY_MS),
        );
        let claimed = harness.project("alpha").join("leftover");
        std::fs::create_dir_all(claimed.join("nested")).expect("the claimed unit");
        std::fs::write(claimed.join("nested/file.txt"), b"bytes").expect("a payload");
        let unclaimed = harness.project("beta").join("leftover");
        std::fs::create_dir_all(&unclaimed).expect("the unclaimed unit");

        harness.delegate(MarkerFamily::OxideScratch);
        harness.delegate(MarkerFamily::EffigyScratch);
        harness.claim("alpha", 60);
        let days = 8;
        let plan = harness.plan(days);
        let mut names = harness.candidate_names(&plan);
        names.sort();
        let mut expected = vec![
            "leftover".to_owned(),
            "dead-run".to_owned(),
            "oxide-proof.old".to_owned(),
        ];
        expected.sort();
        assert_eq!(names, expected, "{:?}", plan.plan.candidates.rows);
        assert_eq!(plan.plan.candidates.count, 3);
        assert!(harness.kept_reason(&plan, "live-run").contains("live"));
        assert!(harness
            .kept_reason(&plan, "oxide-proof.hosting")
            .contains("hosting"));

        let report = harness.expire(days, &plan.plan_digest);
        assert_eq!(report.deleted.count, 3, "{:?}", report.skipped.rows);
        assert!(!eligible_oxide.exists());
        assert!(!eligible_effigy.exists());
        assert!(!claimed.exists());
        assert!(hosting_oxide.is_dir(), "a hosting root is kept");
        assert!(kept_effigy.is_dir(), "a live Effigy pid is kept");
        assert!(unclaimed.is_dir(), "an unclaimed project keeps its units");
        assert_eq!(
            report
                .deleted
                .rows
                .iter()
                .filter(|row| row.marker_removed)
                .count(),
            2,
            "the two marked units unlink their marker last"
        );
    }

    #[test]
    fn an_effigy_marker_omitting_defaulted_fields_is_valid_and_an_extra_field_is_unknown() {
        let harness = Harness::new("scratch-effigy-fields");
        let minimal = harness.scratch().join("minimal");
        std::fs::create_dir_all(&minimal).expect("the run");
        write_marker(
            &minimal.join(super::EFFIGY_MARKER_FILE),
            br#"{"run":"abcdefgh","created_ms":1,"keep":true,"concluded_ms":1}"#,
        );
        let extra = harness.scratch().join("extra");
        std::fs::create_dir_all(&extra).expect("the run");
        write_marker(
            &extra.join(super::EFFIGY_MARKER_FILE),
            br#"{"run":"abcdefgh","created_ms":1,"surprise":true}"#,
        );
        harness.delegate(MarkerFamily::EffigyScratch);
        let days = 8;
        let plan = harness.plan(days);
        // The minimal marker is valid and old, so it is eligible.
        assert!(
            harness
                .candidate_names(&plan)
                .iter()
                .any(|name| name == "minimal"),
            "{:?}",
            plan.plan.candidates.rows
        );
        assert!(
            harness.kept_reason(&plan, "extra").contains("not usable"),
            "{:?}",
            plan.plan.kept.rows
        );
    }

    #[test]
    fn an_oxide_marker_with_an_unknown_field_or_version_is_kept() {
        let harness = Harness::new("scratch-oxide-strict");
        harness.delegate(MarkerFamily::OxideScratch);
        let extra = harness.project("oxide").join("oxide-proof.extra");
        std::fs::create_dir_all(&extra).expect("the root");
        let (device, inode) = identity(&extra);
        let value = serde_json::json!({
            "version": 1, "run_id": "0123456789abcdef0123456789abcdef",
            "root": extra.display().to_string(),
            "owner_uid": crate::os::platform().effective_uid().expect("the uid"),
            "device": device, "inode": inode, "created_unix": 1.0,
            "concluded_unix": 2.0, "outcome": "closed", "closure": "positive",
            "kept_reason": "evidence", "nested_scope": "exclusive",
            "surprise": true
        });
        write_marker(
            &extra.join(super::OXIDE_MARKER_FILE),
            &serde_json::to_vec(&value).unwrap(),
        );
        let future = harness.project("oxide").join("oxide-proof.v2");
        std::fs::create_dir_all(&future).expect("the root");
        let (device, inode) = identity(&future);
        let value = serde_json::json!({
            "version": 2, "run_id": "0123456789abcdef0123456789abcdef",
            "root": future.display().to_string(),
            "owner_uid": crate::os::platform().effective_uid().expect("the uid"),
            "device": device, "inode": inode, "created_unix": 1.0,
            "concluded_unix": 2.0, "outcome": "closed", "closure": "positive",
            "kept_reason": "evidence", "nested_scope": "exclusive"
        });
        write_marker(
            &future.join(super::OXIDE_MARKER_FILE),
            &serde_json::to_vec(&value).unwrap(),
        );
        let plan = harness.plan(8);
        assert!(
            plan.plan.candidates.rows.is_empty(),
            "{:?}",
            plan.plan.candidates
        );
        assert!(harness
            .kept_reason(&plan, "oxide-proof.extra")
            .contains("not usable"));
        assert!(harness
            .kept_reason(&plan, "oxide-proof.v2")
            .contains("version"));
    }

    #[test]
    fn a_claimed_unit_with_a_symlink_inside_is_never_eligible() {
        let harness = Harness::new("scratch-claim-symlink");
        let unit = harness.project("alpha").join("leftover");
        std::fs::create_dir_all(&unit).expect("the unit");
        std::os::unix::fs::symlink("/somewhere", unit.join("link")).expect("the link");
        harness.claim("alpha", 60);
        let plan = harness.plan(1);
        assert!(plan.plan.candidates.rows.is_empty());
        assert!(harness
            .kept_reason(&plan, "leftover")
            .contains("symbolic link"));
    }

    #[test]
    fn the_oxide_project_and_eight_character_names_are_never_inferred() {
        let harness = Harness::new("scratch-reserved");
        let oxide_child = harness.project("oxide").join("unmarked");
        std::fs::create_dir_all(&oxide_child).expect("the oxide child");
        let run_child = harness.project("abcdefgh").join("unmarked");
        std::fs::create_dir_all(&run_child).expect("the run-shaped child");
        // Without any claim, neither name establishes a project: both children
        // are kept, and the eight-character folder itself is reported kept.
        let bare = harness.plan(1);
        assert!(
            bare.plan.candidates.rows.is_empty(),
            "{:?}",
            bare.plan.candidates.rows
        );
        let bare_reason = |path: &Path| -> String {
            bare.plan
                .kept
                .rows
                .iter()
                .find(|row| row.path.as_ref().is_some_and(|known| known == path))
                .map(|row| row.reason.clone())
                .unwrap_or_else(|| panic!("{} is reported kept", path.display()))
        };
        assert!(bare_reason(&run_child).contains("never claimed"));
        assert!(bare_reason(&harness.scratch().join("abcdefgh")).contains("eight-character"));
        // An ad-hoc claim reads the folder itself as the one top-level unit; it is
        // fresh, so it is kept by the claim's own idle age rather than deleted.
        harness.adhoc_claim(14 * 24 * 60 * 60);
        let plan = harness.plan(1);
        assert!(
            plan.plan.candidates.rows.is_empty(),
            "{:?}",
            plan.plan.candidates.rows
        );
        assert!(plan
            .plan
            .candidates
            .rows
            .iter()
            .all(|row| row.name != "unmarked"));
        let reason_at = |path: &Path| -> String {
            plan.plan
                .kept
                .rows
                .iter()
                .find(|row| row.path.as_ref().is_some_and(|known| known == path))
                .map(|row| row.reason.clone())
                .unwrap_or_else(|| panic!("{} is reported kept", path.display()))
        };
        // The folder itself is the covered ad-hoc unit and is fresh, so it is
        // kept by the claim's idle age; its children are not walked at all.
        assert!(reason_at(&harness.scratch().join("abcdefgh"))
            .contains("not older than the claim's idle age"));
        assert!(plan
            .plan
            .candidates
            .rows
            .iter()
            .all(|row| row.name != "abcdefgh"));
        assert!(plan
            .plan
            .kept
            .rows
            .iter()
            .all(|row| row.path.as_ref() != Some(&run_child)));
    }

    #[test]
    fn a_claim_for_the_oxide_project_or_a_dotted_name_is_refused() {
        let harness = Harness::new("scratch-claim-shape");
        for name in ["oxide", "has.dot"] {
            let claim = ScratchClaim {
                scope: ClaimScope::Project {
                    name: name.to_owned(),
                },
                unit: ClaimUnit::Child,
                unmarked_idle_seconds: 60,
                families_excluded: vec![],
                declared_by: "operator".to_owned(),
                declared_ms: state::now_ms(),
            };
            let error = declare_claim(&harness.fixture.state, &harness.fixture.uuid, &claim, false)
                .expect_err("the claim is refused");
            assert_eq!(error.kind(), ErrorKind::Usage, "{name}");
        }
        // A top-level claim declaring the child shape is refused the other way.
        let claim = ScratchClaim {
            scope: ClaimScope::TopLevelAdhoc,
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 60,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        assert_eq!(
            declare_claim(&harness.fixture.state, &harness.fixture.uuid, &claim, false)
                .expect_err("the shape is refused")
                .kind(),
            ErrorKind::Usage
        );
    }

    // ---- deletion mechanics through S5 -------------------------------------

    #[test]
    fn an_unknown_entry_stops_the_delete_and_leaves_a_pending_intent() {
        let harness = Harness::new("scratch-delete-unknown");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        std::fs::write(path.join("payload.txt"), b"bytes").expect("a payload");
        std::fs::write(path.join(".DS_Store"), b"finder").expect("finder metadata");
        std::os::unix::fs::symlink("/elsewhere", path.join("link")).expect("the link");
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        let report = harness.expire(days, &plan.plan_digest);
        assert!(report.has_pending(), "{:?}", report.deleted.rows);
        assert_eq!(report.deleted.count, 0);
        assert_eq!(report.pending.count, 1);
        assert!(
            report.pending.rows[0].reason.contains("symbolic link"),
            "{:?}",
            report.pending.rows
        );
        assert_eq!(
            report.pending.rows[0].phase,
            crate::state::IntentPhase::Renamed
        );
        assert_eq!(report.skipped.count, 0);
    }

    #[test]
    fn a_sweep_that_stops_keeps_its_rows_and_is_never_a_success() {
        let harness = Harness::new("scratch-sweep-stop");
        let first = harness.alloc("alpha", "owner");
        harness.conclude(
            &first,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let second = harness.alloc("beta", "owner");
        harness.conclude(
            &second,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        assert_eq!(plan.plan.candidates.count, 2, "{:?}", plan.plan.candidates);
        let failing = plan.plan.candidates.rows[1].name.clone();

        // The second unit's effect fails in a way that is not that unit's refusal.
        // The sweep stops, and the run it returns carries that stop: the CLI reads
        // `stopped` for its exit code, so a partial sweep can never look like 0.
        // Observation one is the plan's own classification; two is the re-read
        // immediately before the effect.
        let mut faults = Faults::new();
        faults.failing_facts_at(&failing, 2);
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let uuid = harness.fixture.uuid.clone();
        let request = SweepRequest {
            volume_uuid: &uuid,
            preview: false,
            plan_digest: Some(&plan.plan_digest),
            bounds: SweepBounds::default(),
            delete_bounds: DeleteBounds {
                retry_pause: std::time::Duration::ZERO,
                ..DeleteBounds::default()
            },
            now_ms: sweep_now(days, 0),
        };
        let run = sweep(
            &harness.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect("the sweep reports instead of refusing");
        let stopped = run.stopped.as_deref().expect("the stop is reported");
        assert!(stopped.contains("the stage stopped"), "{stopped}");
        assert!(!run.timed_out);
        assert_eq!(run.report.deleted.count, 1, "the first deletion is kept");
        assert_eq!(
            run.report.deleted.rows[0].name,
            plan.plan.candidates.rows[0].name
        );
        assert_eq!(run.report.skipped.count, 1, "the failing unit is reported");
        assert_eq!(run.report.skipped.rows[0].name, failing);
    }

    #[test]
    fn enotempty_leaves_a_pending_intent_and_the_quarantine_leaf() {
        let harness = Harness::new("scratch-delete-enotempty");
        let path = harness.alloc("project-a", "owner");
        harness.conclude(
            &path,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        std::fs::write(path.join("payload.txt"), b"bytes").expect("a payload");
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        let faults = Faults::new();
        faults.failing_remove_prefix(state::CAPTURE_PREFIX, 3);
        let report = harness.expire_with(&faults, days, &plan.plan_digest);
        assert!(report.has_pending(), "{:?}", report.deleted.rows);
        assert_eq!(
            report.pending.rows[0].phase,
            crate::state::IntentPhase::MarkerLast
        );
        assert!(report.pending.rows[0].reason.contains("not empty"));
    }

    #[test]
    fn an_expire_plan_stage_starts_no_effect_once_its_deadline_is_reached() {
        let harness = Harness::new("scratch-stage-deadline");
        let root = harness.alloc("deadline", "owner");
        harness.conclude(
            &root,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let preview = harness.plan(days);
        assert_eq!(preview.plan.candidates.count, 1);

        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        // A caller whose clock is already spent: the replay never starts, the
        // planned unit is never attempted, and every planned unit is reported.
        let run = expire_plan(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &harness.fixture.record,
            &preview.plan,
            harness.fixture.record.generation,
            DeleteBounds {
                retry_pause: std::time::Duration::ZERO,
                ..DeleteBounds::default()
            },
            sweep_now(days, 0),
            Some(std::time::Instant::now()),
        )
        .expect("the stage reports");
        assert!(run.timed_out, "{run:?}");
        assert_eq!(run.unreached, preview.plan.candidates.count);
        assert_eq!(run.report.deleted.count, 0);
        assert_eq!(run.report.pending.count, 0);
        assert_eq!(run.report.skipped.count, 1);
        assert!(
            run.report.skipped.rows[0].reason.contains("wall clock"),
            "{}",
            run.report.skipped.rows[0].reason
        );
        assert!(run.report.replay.is_none(), "no replay starts either");
        assert!(root.is_dir(), "the planned unit is kept");
    }

    #[test]
    fn an_expire_plan_stage_with_no_candidates_reports_a_spent_clock() {
        let harness = Harness::new("scratch-stage-deadline-empty");
        // A run that is not old enough to be eligible: the plan has no candidate
        // at all, which is the shape that must still report a timeout.
        let _root = harness.alloc("fresh", "owner");
        let preview = harness.plan(0);
        assert_eq!(preview.plan.candidates.count, 0);

        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let run = expire_plan(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &harness.fixture.record,
            &preview.plan,
            harness.fixture.record.generation,
            DeleteBounds {
                retry_pause: std::time::Duration::ZERO,
                ..DeleteBounds::default()
            },
            sweep_now(0, 0),
            Some(std::time::Instant::now()),
        )
        .expect("the stage reports");
        assert!(run.timed_out, "an empty plan must not hide a spent clock");
        assert_eq!(run.unreached, 0);
        assert_eq!(run.report.deleted.count, 0);
        assert_eq!(run.report.pending.count, 0);
        assert!(
            run.report.replay.is_none(),
            "no replay starts on a spent clock"
        );
    }

    #[test]
    fn an_expire_plan_stage_on_a_kept_volume_keeps_every_planned_unit() {
        let harness = Harness::new("scratch-stage-kept");
        let root = harness.alloc("kept", "owner");
        harness.conclude(
            &root,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let preview = harness.plan(days);
        assert_eq!(preview.plan.candidates.count, 1);

        // The record now declares the volume kept, at the same generation: a plan
        // made for an expendable volume may never prune a volume that says its data
        // is kept.
        let mut kept_record = harness.fixture.record.clone();
        kept_record.durability = crate::registry::Durability::Kept;
        assert_eq!(kept_record.generation, harness.fixture.record.generation);
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let run = expire_plan(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &kept_record,
            &preview.plan,
            harness.fixture.record.generation,
            DeleteBounds {
                retry_pause: std::time::Duration::ZERO,
                ..DeleteBounds::default()
            },
            sweep_now(days, 0),
            None,
        )
        .expect("the stage reports");
        assert_eq!(run.report.deleted.count, 0);
        assert_eq!(run.report.pending.count, 0);
        assert_eq!(run.report.skipped.count, 1);
        assert!(
            run.report.skipped.rows[0]
                .reason
                .contains("never permits pruning"),
            "{}",
            run.report.skipped.rows[0].reason
        );
        assert!(!run.timed_out);
        assert!(root.is_dir(), "the planned unit is kept");
    }

    #[test]
    fn a_permanent_volume_that_is_absent_refuses_and_writes_nothing() {
        let harness = Harness::new("scratch-volume-absent");
        let facts = harness.fixture.facts();
        facts.present.set(false);
        let mounts = harness.fixture.mounts();
        let observer = harness.fixture.observer();
        let platform = harness.fixture.platform();
        let uuid = harness.fixture.uuid.clone();
        let request = SweepRequest {
            volume_uuid: &uuid,
            preview: true,
            plan_digest: None,
            bounds: SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            now_ms: sweep_now(8, 0),
        };
        let error = sweep(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect_err("a permanent volume's absence is loud");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
        assert!(
            !harness.fixture.root.join("scratch").exists(),
            "a stale mount point is never written to"
        );
    }

    #[test]
    fn an_ephemeral_volume_that_is_absent_is_reported_and_writes_nothing() {
        let harness = Harness::new("scratch-volume-ephemeral");
        let mut record = harness.fixture.record.clone();
        record.availability = crate::registry::Availability::Ephemeral;
        harness.fixture.register_record(&record);
        let facts = harness.fixture.facts();
        facts.present.set(false);
        let mounts = harness.fixture.mounts();
        let observer = harness.fixture.observer();
        let platform = harness.fixture.platform();
        let uuid = harness.fixture.uuid.clone();
        let request = SweepRequest {
            volume_uuid: &uuid,
            preview: true,
            plan_digest: None,
            bounds: SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            now_ms: sweep_now(8, 0),
        };
        let report = sweep(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect("an ephemeral volume's absence is a report")
        .report;
        assert!(!report.volume_present);
        assert_eq!(report.plan.candidates.count, 0);
        assert!(!harness.fixture.root.join("scratch").exists());
    }

    #[test]
    fn nested_markers_keep_native_and_delegated_roots_out_of_the_plan() {
        for outer in [
            UnitFamily::Native,
            UnitFamily::OxideScratch,
            UnitFamily::EffigyScratch,
        ] {
            for marker in crate::unit::RECOGNISED_MARKER_FILES {
                let harness = Harness::new("nested-marked-root");
                let root = match outer {
                    UnitFamily::Native => {
                        let root = harness.alloc("alpha", "owner");
                        harness.conclude(
                            &root,
                            "owner",
                            ConclusionOutcome::Closed,
                            ConclusionClosure::Positive,
                            KeptReason::Evidence,
                            NestedScope::Exclusive,
                        );
                        root
                    }
                    UnitFamily::OxideScratch => {
                        let root = harness.project("oxide").join("oxide-proof.old");
                        std::fs::create_dir_all(&root).expect("oxide root");
                        oxide_marker(
                            &root,
                            "exclusive",
                            "evidence",
                            Some(state::now_ms() - 30 * DAY_MS),
                        );
                        harness.delegate(MarkerFamily::OxideScratch);
                        root
                    }
                    UnitFamily::EffigyScratch => {
                        let root = harness.scratch().join("run.effigy");
                        std::fs::create_dir_all(&root).expect("effigy root");
                        effigy_marker(
                            &root,
                            0xFFFF_FFFE,
                            true,
                            Some(state::now_ms() - 30 * DAY_MS),
                        );
                        harness.delegate(MarkerFamily::EffigyScratch);
                        root
                    }
                    UnitFamily::Unmarked => unreachable!(),
                };
                let nested = root.join("inner-run/deeper");
                std::fs::create_dir_all(&nested).expect("nested root");
                std::fs::write(nested.join(marker), b"foreign").expect("nested marker");
                let plan = harness.plan(8);
                assert_eq!(plan.plan.candidates.count, 0, "{outer:?} {marker}");
                let name = root.file_name().unwrap().to_string_lossy();
                assert!(harness
                    .kept_reason(&plan, &name)
                    .contains(&format!("inner-run/deeper/{marker}")));
                let real = harness.expire(8, &plan.plan_digest);
                assert_eq!(real.deleted.count, 0);
                assert_eq!(real.pending.count, 0);
                assert!(nested.join(marker).exists());
            }
        }
    }

    #[test]
    fn a_bounded_marked_root_walk_keeps_the_root_and_reports_truncation() {
        let harness = Harness::new("marked-root-walk-bound");
        let root = harness.scratch().join("run.effigy");
        std::fs::create_dir_all(&root).expect("root");
        effigy_marker(
            &root,
            0xFFFF_FFFE,
            true,
            Some(state::now_ms() - 30 * DAY_MS),
        );
        for index in 0..20 {
            std::fs::write(root.join(format!("payload-{index}")), b"x").expect("payload");
        }
        harness.delegate(MarkerFamily::EffigyScratch);
        let request = SweepRequest {
            volume_uuid: &harness.fixture.uuid,
            preview: true,
            plan_digest: None,
            bounds: SweepBounds {
                examine: 8,
                timeout: super::SWEEP_TIMEOUT,
            },
            delete_bounds: DeleteBounds::default(),
            now_ms: state::now_ms(),
        };
        let report = sweep(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &request,
        )
        .expect("preview")
        .report;
        assert!(report.plan.truncated);
        assert_eq!(report.plan.candidates.count, 0);
        assert!(harness
            .kept_reason(&report, "run.effigy")
            .contains("budget"));
        assert!(root.exists());
    }

    #[test]
    fn nested_markers_keep_claimed_units_even_with_family_delegations() {
        for top_level in [false, true] {
            for delegated in [false, true] {
                for marker in crate::unit::RECOGNISED_MARKER_FILES {
                    let harness = Harness::new("nested-marker-claim");
                    let unit = if top_level {
                        std::fs::create_dir_all(harness.scratch()).expect("scratch");
                        harness.adhoc_claim(60);
                        harness.scratch().join("run.legacy")
                    } else {
                        harness.claim("alpha", 60);
                        harness.project("alpha").join("old-child")
                    };
                    if delegated {
                        harness.delegate(MarkerFamily::EffigyScratch);
                        harness.delegate(MarkerFamily::OxideScratch);
                    }
                    let nested = unit.join("inner-run/deeper");
                    std::fs::create_dir_all(&nested).expect("nested directory");
                    let marker_path = nested.join(marker);
                    std::fs::write(&marker_path, b"foreign ownership").expect("marker");
                    age_tree(&unit, 20);
                    let now = state::now_ms() + 3_600_000;
                    let plan = harness.plan_at(now);
                    assert_eq!(
                        plan.plan.candidates.count, 0,
                        "{top_level} {delegated} {marker}"
                    );
                    let name = unit.file_name().unwrap().to_string_lossy();
                    let kept = plan
                        .plan
                        .kept
                        .rows
                        .iter()
                        .find(|row| row.name == name)
                        .expect("unit reported");
                    assert!(
                        kept.reason.contains(&format!("inner-run/deeper/{marker}")),
                        "{}",
                        kept.reason
                    );
                    let real = harness.expire_at(now, &plan.plan_digest);
                    assert_eq!(real.deleted.count, 0);
                    assert!(marker_path.is_file(), "nested marker is kept");
                }
            }
        }
    }

    #[test]
    fn a_nested_marker_inserted_after_preview_refuses_locked_reassessment() {
        let harness = Harness::new("nested-marker-after-preview");
        let unit = harness.project("alpha").join("old-child");
        let nested = unit.join("inner-run");
        std::fs::create_dir_all(&nested).expect("nested directory");
        age_tree(&unit, 20);
        harness.claim("alpha", 60);
        let now = state::now_ms() + 3_600_000;
        let plan = harness.plan_at(now);
        let row = &plan.plan.candidates.rows[0];
        std::fs::write(nested.join(super::EFFIGY_MARKER_FILE), b"foreign").expect("marker");
        let record = area::registered_volume(&harness.fixture.state, &harness.fixture.uuid)
            .expect("registry");
        let error = super::authorize_under(
            &harness.fixture.platform(),
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.fixture.state,
            &record,
            record.generation,
            row,
            now,
        )
        .expect_err("the new marker refuses");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        let request = SweepRequest {
            volume_uuid: &harness.fixture.uuid,
            preview: false,
            plan_digest: Some(&plan.plan_digest),
            bounds: SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            now_ms: now,
        };
        let result = sweep(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &request,
        );
        assert_eq!(
            result.expect_err("the stale preview refuses").kind(),
            ErrorKind::UnknownState
        );
        assert!(nested.join(super::EFFIGY_MARKER_FILE).exists());
    }

    #[test]
    fn a_claimed_top_level_file_is_deleted_whole_through_s5() {
        let harness = Harness::new("scratch-claimed-file");
        std::fs::create_dir_all(harness.scratch()).expect("the scratch folder");
        let file = harness.scratch().join("val.ABC123");
        std::fs::write(&file, b"payload").expect("the ad-hoc file");
        age_tree(&file, 20);
        harness.adhoc_claim(60);
        let days = 1;
        let plan = harness.plan(days);
        assert_eq!(plan.plan.candidates.count, 1);
        assert_eq!(plan.plan.candidates.rows[0].family, UnitFamily::Unmarked);
        assert!(plan.plan.candidates.rows[0].marker_file.is_none());

        let report = harness.expire(days, &plan.plan_digest);
        assert_eq!(report.deleted.count, 1, "{:?}", report.skipped.rows);
        let row = &report.deleted.rows[0];
        assert!(!row.marker_removed, "an unmarked entry has no marker");
        assert_eq!(row.directories_removed, 0);
        assert!(!file.exists(), "the file unit is gone");
        let intents = state::read_intents(&harness.fixture.state, 64).expect("the journal");
        assert!(intents.pending().next().is_none());
    }

    #[test]
    fn a_real_run_replays_a_pending_intent_before_its_own_effects() {
        let harness = Harness::new("scratch-replay");
        let root = harness.alloc("project-a", "owner");
        harness.conclude(
            &root,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        std::fs::write(root.join("payload.txt"), b"bytes").expect("a payload");
        std::os::unix::fs::symlink("/elsewhere", root.join("link")).expect("the link");
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        let stopped = harness.expire(days, &plan.plan_digest);
        assert!(stopped.has_pending(), "{:?}", stopped.deleted.rows);

        // The operator resolves the obstacle the delete refused: the link goes,
        // and the next real run replays the intent from its recorded identity.
        let quarantine = std::fs::read_dir(harness.project("project-a"))
            .expect("the project folder")
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(state::QUARANTINE_PREFIX)
            })
            .expect("the quarantine leaf")
            .path();
        std::fs::remove_file(quarantine.join("link")).expect("the obstacle is removed");
        let empty = harness.plan(days);
        assert_eq!(
            empty.plan.candidates.count, 0,
            "a quarantine leaf is never a candidate"
        );
        let report = harness.expire(days, &empty.plan_digest);
        let replay = report
            .replay
            .as_ref()
            .expect("a real run reports its replay");
        assert!(
            replay.settled + replay.resumed == 1,
            "the replay did not settle or resume the intent: {replay:?}"
        );
        assert!(!quarantine.exists(), "the replay finished the deletion");
        let intents = state::read_intents(&harness.fixture.state, 64).expect("the journal");
        assert!(intents.pending().next().is_none(), "nothing stays pending");
    }

    // ---- the host reaper's counterfactual baseline --------------------------

    /// The host reaper's deletable set, exactly as `reap.py` computes it: children
    /// of each project folder and the scratch root's ad-hoc entries, once the
    /// newest entry is fourteen days old and no process has it open. The fixture
    /// keeps no process holding any unit, so "busy" never holds here.
    fn reaper_units(scratch: &Path, now_ms: u64) -> Vec<PathBuf> {
        const IDLE_MS: u64 = 14 * 24 * 60 * 60 * 1000;
        let mut units: Vec<PathBuf> = Vec::new();
        let mut names: Vec<String> = std::fs::read_dir(scratch)
            .expect("the scratch folder")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        for name in names {
            let path = scratch.join(&name);
            let metadata = std::fs::symlink_metadata(&path).expect("the entry");
            if metadata.file_type().is_symlink() || name == "oxide" {
                continue;
            }
            if path.join(".effigy-scratch.json").exists() {
                continue;
            }
            if metadata.is_dir() && !name.contains('.') {
                let mut children: Vec<String> = std::fs::read_dir(&path)
                    .expect("the project folder")
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect();
                children.sort();
                units.extend(
                    children
                        .into_iter()
                        .map(|child| path.join(child))
                        .filter(|child| !child.join(".effigy-scratch.json").exists()),
                );
            } else {
                units.push(path);
            }
        }
        let newest = |path: &Path| -> u64 {
            let mut newest = 0u64;
            let mut stack = vec![path.to_path_buf()];
            while let Some(current) = stack.pop() {
                let metadata = std::fs::symlink_metadata(&current).expect("the entry");
                newest = newest.max(
                    metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |elapsed| elapsed.as_millis() as u64),
                );
                if metadata.is_dir() {
                    for entry in std::fs::read_dir(&current).expect("the folder").flatten() {
                        stack.push(entry.path());
                    }
                }
            }
            newest
        };
        units.retain(|path| {
            let metadata = std::fs::symlink_metadata(path).expect("the unit");
            if metadata.file_type().is_symlink() {
                return false;
            }
            now_ms.saturating_sub(newest(path)) >= IDLE_MS
        });
        units.sort();
        units
    }

    #[test]
    fn under_grants_the_sweep_reaches_the_reapers_deletable_set() {
        let harness = Harness::new("scratch-reaper-equivalence");
        let scratch = harness.scratch();
        std::fs::create_dir_all(&scratch).expect("the scratch folder");

        // Old project children, one fresh child and one symlink child.
        let alpha_old = harness.project("alpha").join("old-child");
        std::fs::create_dir_all(alpha_old.join("nested")).expect("the unit");
        std::fs::write(alpha_old.join("nested/payload.txt"), b"bytes").expect("a payload");
        age_tree(&alpha_old, 20);
        let alpha_fresh = harness.project("alpha").join("fresh-child");
        std::fs::create_dir_all(&alpha_fresh).expect("the fresh unit");
        std::os::unix::fs::symlink("/elsewhere", harness.project("alpha").join("link-child"))
            .expect("the link");
        let beta_old = harness.project("beta").join("old-child");
        std::fs::create_dir_all(&beta_old).expect("the unit");
        age_tree(&beta_old, 20);

        // Ad-hoc entries: a file and a dotted directory, both one unit each.
        let adhoc_file = scratch.join("file-old.txt");
        std::fs::write(&adhoc_file, b"bytes").expect("the adhoc file");
        age_tree(&adhoc_file, 20);
        let adhoc_dotted = scratch.join("val.ABC123");
        std::fs::create_dir_all(adhoc_dotted.join("inner")).expect("the dotted unit");
        age_tree(&adhoc_dotted, 20);

        // The Oxide project and the marked Effigy root are skipped by the reaper.
        let oxide_child = harness.project("oxide").join("old-child");
        std::fs::create_dir_all(&oxide_child).expect("the oxide child");
        age_tree(&oxide_child, 20);
        let run_child = harness.project("abcdefgh").join("old-child");
        std::fs::create_dir_all(&run_child).expect("the run-shaped child");
        age_tree(&run_child, 20);
        let validation = scratch.join("oxide-validation/run.1");
        std::fs::create_dir_all(&validation).expect("the validation temp");
        age_tree(&validation, 20);
        let effigy = scratch.join("marked-run");
        std::fs::create_dir_all(&effigy).expect("the effigy run");
        effigy_marker(&effigy, 0xFFFF_FFFE, false, None);
        age_tree(&effigy, 20);
        // Plain eight-character project names have no special treatment in the
        // current host reaper; their old children are units it deletes.
        let underlay_old = harness.project("underlay").join("old-child");
        std::fs::create_dir_all(&underlay_old).expect("the unit");
        std::fs::write(underlay_old.join("payload.txt"), b"bytes").expect("a payload");
        age_tree(&underlay_old, 20);
        // A second named project and the arbitrary `abcdefgh` above prove the
        // model does not rely on a known-names list.
        let figmatic_old = harness.project("figmatic").join("old-child");
        std::fs::create_dir_all(&figmatic_old).expect("the unit");
        age_tree(&figmatic_old, 20);

        let now = state::now_ms() + 3_600_000;
        let reaper_all = reaper_units(&scratch, now);
        // The host reaper descends into `oxide-validation` and would delete the
        // validation temp; the design keeps it, which is Barge's extra
        // conservatism and not a difference in the compared set.
        assert!(
            reaper_all.iter().any(|unit| unit.starts_with(&validation)),
            "the reaper would have deleted Oxide's validation temp: {reaper_all:?}"
        );
        let reaper: Vec<PathBuf> = reaper_all
            .into_iter()
            .filter(|unit| !unit.starts_with(&validation))
            .collect();

        // No grants: nothing is deleted and everything the reaper would delete is
        // reported as kept.
        let bare = harness.plan_at(now);
        assert!(
            bare.plan.candidates.rows.is_empty(),
            "{:?}",
            bare.plan.candidates.rows
        );
        for unit in &reaper {
            let name = unit.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                bare.plan.kept.rows.iter().any(|row| row.name == name),
                "{} is reported kept",
                unit.display()
            );
        }

        // The design stages the reaper's replacement as a sequence of recorded
        // declarations, and an eight-character project claim and a top-level
        // ad-hoc claim may not cover the same name at once. The first declaration
        // set is the ad-hoc claim plus the plain project claims.
        harness.claim("alpha", 14 * 24 * 60 * 60);
        harness.claim("beta", 14 * 24 * 60 * 60);
        harness.adhoc_claim(14 * 24 * 60 * 60);
        let granted = harness.plan_at(now);
        let eight_character: Vec<PathBuf> = reaper
            .iter()
            .filter(|unit| {
                unit.parent().and_then(Path::file_name).is_some_and(|name| {
                    name == "underlay" || name == "figmatic" || name == "abcdefgh"
                })
            })
            .cloned()
            .collect();
        assert_eq!(
            eight_character.len(),
            3,
            "all eight-character projects are in the reaper's set: {reaper:?}"
        );
        let mut first_expected: Vec<PathBuf> = reaper
            .iter()
            .filter(|unit| !eight_character.contains(unit))
            .cloned()
            .collect();
        first_expected.sort();
        let mut barge: Vec<PathBuf> = granted
            .plan
            .candidates
            .rows
            .iter()
            .map(|row| row.path.clone())
            .collect();
        barge.sort();
        assert_eq!(
            barge, first_expected,
            "the first granted sweep matches the reaper outside the eight-character projects"
        );

        // The same run deletes exactly that set, including the ad-hoc file.
        let real = harness.expire_at(now, &granted.plan_digest);
        assert_eq!(
            real.deleted.count,
            first_expected.len(),
            "{:?} {:?}",
            real.deleted.rows,
            real.skipped.rows
        );
        for unit in &first_expected {
            assert!(!unit.exists(), "{} was deleted", unit.display());
        }

        // The second declaration set withdraws the ad-hoc claim and records the
        // explicit eight-character project claims: together the two sets reach
        // the reaper's whole deletable set, and neither deletes anything else.
        let adhoc = ScratchClaim {
            scope: ClaimScope::TopLevelAdhoc,
            unit: ClaimUnit::Entry,
            unmarked_idle_seconds: 14 * 24 * 60 * 60,
            families_excluded: vec![MarkerFamily::OxideScratch, MarkerFamily::EffigyScratch],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        declare_claim(&harness.fixture.state, &harness.fixture.uuid, &adhoc, true)
            .expect("the ad-hoc claim is withdrawn");
        harness.claim("underlay", 14 * 24 * 60 * 60);
        harness.claim("figmatic", 14 * 24 * 60 * 60);
        harness.claim("abcdefgh", 14 * 24 * 60 * 60);
        let second = harness.plan_at(now);
        let mut second_barge: Vec<PathBuf> = second
            .plan
            .candidates
            .rows
            .iter()
            .map(|row| row.path.clone())
            .collect();
        second_barge.sort();
        assert_eq!(
            second_barge, eight_character,
            "the explicit eight-character project claims reach exactly the reaper's branch"
        );
        let second_real = harness.expire_at(now, &second.plan_digest);
        assert_eq!(
            second_real.deleted.count,
            eight_character.len(),
            "{:?} {:?}",
            second_real.deleted.rows,
            second_real.skipped.rows
        );
        for unit in &reaper {
            assert!(!unit.exists(), "{} was deleted", unit.display());
        }
        assert!(!underlay_old.exists() && !figmatic_old.exists());

        // Each condition the design lists is at least as conservative: every
        // reaper skip is also a Barge keep, and the validation temp is kept.
        assert!(
            granted
                .plan
                .kept
                .rows
                .iter()
                .any(|row| row.name == super::OXIDE_VALIDATION_DIR),
            "Oxide's validation temp is reported kept"
        );
        for kept in [&oxide_child, &run_child, &effigy, &alpha_fresh] {
            let name = kept.file_name().unwrap().to_string_lossy().to_string();
            let reported = granted.plan.kept.rows.iter().any(|row| row.name == name)
                || granted
                    .plan
                    .kept
                    .rows
                    .iter()
                    .any(|row| row.path.as_ref().is_some_and(|path| path.starts_with(kept)));
            assert!(reported, "{} is reported kept by Barge", kept.display());
        }
        // The fresh child is kept by Barge too, because its newest entry is new.
        assert!(granted
            .plan
            .kept
            .rows
            .iter()
            .any(|row| row.name == "fresh-child"));
    }

    /// A directory-operations wrapper that rewrites one marker immediately after
    /// the first read of it, so a test can make the world change between a
    /// plan's read and the effect's re-verification.
    struct RaceOnce {
        inner: crate::os::MacOs,
        marker: PathBuf,
        expected: Vec<u8>,
        remaining: std::cell::Cell<usize>,
    }

    impl RaceOnce {
        fn new(marker: PathBuf) -> Self {
            let expected = std::fs::read(&marker).expect("the racing marker");
            Self {
                inner: crate::os::platform(),
                marker,
                expected,
                // The real run reads the marker once to identify the unit and
                // once to assess it; the change lands immediately after both, so
                // the plan still matches the preview and only the effect's
                // re-verification sees the new identity.
                remaining: std::cell::Cell::new(2),
            }
        }
    }

    impl DirOps for RaceOnce {
        fn open_path(&self, path: &Path) -> crate::error::Result<DirHandle> {
            self.inner.open_path(path)
        }

        fn path_facts(&self, path: &Path) -> crate::error::Result<Option<crate::os::EntryFacts>> {
            self.inner.path_facts(path)
        }

        fn open_child_dir(
            &self,
            parent: &DirHandle,
            name: &str,
        ) -> crate::error::Result<DirHandle> {
            self.inner.open_child_dir(parent, name)
        }

        fn duplicate_dir(&self, handle: &DirHandle) -> crate::error::Result<DirHandle> {
            self.inner.duplicate_dir(handle)
        }

        fn child_facts(
            &self,
            parent: &DirHandle,
            name: &str,
        ) -> crate::error::Result<Option<crate::os::EntryFacts>> {
            self.inner.child_facts(parent, name)
        }

        fn create_child_dir(
            &self,
            parent: &DirHandle,
            name: &str,
            mode: u32,
        ) -> crate::error::Result<crate::os::DirId> {
            self.inner.create_child_dir(parent, name, mode)
        }

        fn create_child_symlink(
            &self,
            parent: &DirHandle,
            name: &str,
            target: &Path,
        ) -> crate::error::Result<()> {
            self.inner.create_child_symlink(parent, name, target)
        }

        fn read_link(&self, path: &Path) -> crate::error::Result<Option<std::path::PathBuf>> {
            self.inner.read_link(path)
        }

        fn publish_child_file(
            &self,
            parent: &DirHandle,
            name: &str,
            bytes: &[u8],
            mode: u32,
        ) -> crate::error::Result<()> {
            self.inner.publish_child_file(parent, name, bytes, mode)
        }

        fn read_child_file(
            &self,
            parent: &DirHandle,
            name: &str,
            cap: usize,
        ) -> crate::error::Result<Option<Vec<u8>>> {
            let bytes = self.inner.read_child_file(parent, name, cap)?;
            if name == area::ROOT_MARKER_FILE
                && bytes.as_deref() == Some(self.expected.as_slice())
                && self.remaining.get() > 0
            {
                let left = self.remaining.get() - 1;
                self.remaining.set(left);
                if left > 0 {
                    return Ok(bytes);
                }
                let mut value: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(&self.marker).expect("the racing marker"),
                )
                .expect("the racing marker parses");
                value["nonce"] = serde_json::json!("0123456789abcdef0123456789abcdef");
                write_marker(&self.marker, &serde_json::to_vec(&value).unwrap());
            }
            Ok(bytes)
        }

        fn list_children(
            &self,
            parent: &DirHandle,
            max_names: usize,
        ) -> crate::error::Result<crate::os::ChildListing> {
            self.inner.list_children(parent, max_names)
        }

        fn list_names(
            &self,
            parent: &DirHandle,
            max_names: usize,
        ) -> crate::error::Result<crate::os::DirNames> {
            self.inner.list_names(parent, max_names)
        }

        fn remove_child(
            &self,
            parent: &DirHandle,
            name: &str,
            kind: crate::os::RemoveKind,
        ) -> crate::error::Result<crate::os::ChildRemoval> {
            self.inner.remove_child(parent, name, kind)
        }

        fn rename_child(
            &self,
            parent: &DirHandle,
            from: &str,
            to: &str,
        ) -> crate::error::Result<crate::os::RenameOutcome> {
            self.inner.rename_child(parent, from, to)
        }

        fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> crate::error::Result<()> {
            self.inner.set_dir_mode(handle, mode)
        }
        fn dir_facts(&self, handle: &DirHandle) -> crate::error::Result<crate::os::EntryFacts> {
            self.inner.dir_facts(handle)
        }
        fn recheck_dir(&self, handle: &DirHandle) -> crate::error::Result<crate::os::DirId> {
            self.inner.recheck_dir(handle)
        }

        fn random_bytes(&self, length: usize) -> crate::error::Result<Vec<u8>> {
            self.inner.random_bytes(length)
        }
    }

    /// Age a tree's entries by rewriting their mtimes through `find` and
    /// `touch`, which is the only portable way for a test to move a file's
    /// modification time without an unsafe libc call.
    /// Set every entry's mtime to `age` before now, so a policy whose window is
    /// between two ages can be tested exactly.
    fn set_tree_age(path: &Path, age: std::time::Duration) {
        let stamp = std::time::SystemTime::now() - age;
        let mut stack = vec![path.to_path_buf()];
        while let Some(current) = stack.pop() {
            let file = std::fs::File::open(&current).expect("the entry opens");
            file.set_modified(stamp).expect("the mtime is set");
            if std::fs::symlink_metadata(&current)
                .expect("the entry's facts")
                .is_dir()
            {
                for entry in std::fs::read_dir(&current).expect("the folder").flatten() {
                    stack.push(entry.path());
                }
            }
        }
    }

    fn age_tree(path: &Path, _days: u64) {
        let status = std::process::Command::new("find")
            .arg(path)
            .arg("-exec")
            .arg("touch")
            .arg("-t")
            .arg("202001010000.00")
            .arg("{}")
            .arg("+")
            .status()
            .expect("find runs");
        assert!(status.success(), "aging {} failed", path.display());
    }

    // ---- foreign marker readers -------------------------------------------

    #[test]
    fn a_family_marker_reader_reports_absent_present_and_unknown() {
        let harness = Harness::new("scratch-family-reader");
        let dir = harness.scratch().join("reader");
        std::fs::create_dir_all(&dir).expect("the directory");
        let platform = harness.fixture.platform();
        let observer = harness.fixture.observer();
        let handle: DirHandle = platform.open_path(&dir).expect("the handle");
        let reading: MarkerReading<super::EffigyScratchMarker> = super::read_family_marker(
            &platform,
            &observer,
            &handle,
            super::EFFIGY_MARKER_FILE,
            false,
        );
        assert_eq!(reading, MarkerReading::Absent);

        write_marker(
            &dir.join(super::EFFIGY_MARKER_FILE),
            br#"{"run":"x","created_ms":5}"#,
        );
        let reading: MarkerReading<super::EffigyScratchMarker> = super::read_family_marker(
            &platform,
            &observer,
            &handle,
            super::EFFIGY_MARKER_FILE,
            false,
        );
        match reading {
            MarkerReading::Present { marker, digest } => {
                assert_eq!(marker.run, "x");
                assert_eq!(marker.task, "");
                assert_eq!(digest.len(), 64);
            }
            other => panic!("{other:?}"),
        }

        write_marker(&dir.join(super::EFFIGY_MARKER_FILE), b"not json");
        let reading: MarkerReading<super::EffigyScratchMarker> = super::read_family_marker(
            &platform,
            &observer,
            &handle,
            super::EFFIGY_MARKER_FILE,
            false,
        );
        assert!(matches!(reading, MarkerReading::Unknown { .. }));

        // An Oversized marker is unknown without reading its body.
        write_marker(
            &dir.join(super::EFFIGY_MARKER_FILE),
            &vec![b'a'; MARKER_CAP_BYTES + 1],
        );
        let reading: MarkerReading<super::EffigyScratchMarker> = super::read_family_marker(
            &platform,
            &observer,
            &handle,
            super::EFFIGY_MARKER_FILE,
            false,
        );
        assert!(matches!(reading, MarkerReading::Unknown { .. }));
    }

    #[test]
    fn the_unmarked_provenance_digest_is_stable_and_identity_bound() {
        let uuid = crate::os::VolumeUuid::parse(FIXTURE_UUID).expect("the UUID");
        let first = unmarked_provenance_digest_parts(&uuid, "leftover", 7, 11);
        let second = unmarked_provenance_digest_parts(&uuid, "leftover", 7, 11);
        assert_eq!(first, second);
        assert_ne!(
            first,
            unmarked_provenance_digest_parts(&uuid, "leftover", 7, 12)
        );
        let policy = SweepPolicy::design();
        assert_eq!(policy.native_age_seconds, NATIVE_AGE_SECONDS);
        assert_eq!(policy.oxide_age_seconds, OXIDE_AGE_SECONDS);
        assert_eq!(policy.effigy_keep_age_seconds, EFFIGY_KEEP_AGE_SECONDS);
        assert_eq!(policy.effigy_age_seconds, EFFIGY_AGE_SECONDS);
        assert_eq!(UnitFamily::Native.grant(), "native_root");
        assert_eq!(UnitFamily::OxideScratch.grant(), "delegation");
        assert_eq!(UnitFamily::Unmarked.grant(), "legacy_claim");
    }

    #[test]
    fn a_scratch_declaration_never_leaves_the_folder_it_names() {
        let harness = Harness::new("scratch-declaration");
        let declaration = super::scratch_declaration(&harness.fixture.record, "scratch/alpha");
        declaration.validate().expect("the declaration is valid");
        assert_eq!(declaration.namespace, super::SCRATCH_NAMESPACE);
        assert_eq!(declaration.area, super::SCRATCH_AREA);
        assert_eq!(declaration.folder, "scratch/alpha");
        super::scratch_declaration(&harness.fixture.record, "../escape")
            .validate()
            .expect_err("a folder that escapes is refused");
    }

    #[test]
    fn a_symlink_unit_is_never_followed_or_deleted() {
        let harness = Harness::new("scratch-symlink-unit");
        let outside = harness.fixture.dir.join("outside");
        std::fs::create_dir_all(&outside).expect("the outside directory");
        std::fs::write(outside.join("precious.txt"), b"bytes").expect("the precious file");
        std::os::unix::fs::symlink(&outside, harness.project("alpha").join("link"))
            .expect("the link");
        harness.claim("alpha", 60);
        let plan = harness.plan(1);
        assert!(plan.plan.candidates.rows.is_empty());
        assert!(harness.kept_reason(&plan, "link").contains("symbolic link"));
        assert!(outside.join("precious.txt").is_file());
    }

    #[test]
    fn a_sweep_bound_reports_what_it_did_not_reach() {
        let harness = Harness::new("scratch-bound");
        for index in 0..4 {
            let unit = harness.project("alpha").join(format!("leftover-{index}"));
            std::fs::create_dir_all(&unit).expect("the unit");
        }
        harness.claim("alpha", 60);
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let uuid = harness.fixture.uuid.clone();
        let request = SweepRequest {
            volume_uuid: &uuid,
            preview: true,
            plan_digest: None,
            bounds: SweepBounds {
                examine: 2,
                timeout: super::SWEEP_TIMEOUT,
            },
            delete_bounds: DeleteBounds::default(),
            now_ms: sweep_now(1, 0),
        };
        let report = sweep(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect("the bounded preview reports")
        .report;
        assert!(report.plan.truncated, "the bound is reported");
        assert!(report.plan.examined <= 2);
    }

    // ---- the corrected grant, conclusion and replay seams ------------------

    #[test]
    fn the_locked_authorization_rechecks_the_full_current_policy() {
        let harness = Harness::new("scratch-locked-policy");
        let idle = 14 * 24 * 60 * 60;
        // An unmarked child in a claimed project, fifteen days old at `now`.
        let unit = harness.project("alpha").join("leftover");
        std::fs::create_dir_all(&unit).expect("the unit");
        std::fs::write(unit.join("payload.txt"), b"bytes").expect("a payload");
        set_tree_age(&unit, std::time::Duration::from_secs(15 * 24 * 60 * 60));
        // An Effigy-marked root in the same claimed project, old enough for its
        // family's rule.
        let marked = harness.project("alpha").join("marked-run");
        std::fs::create_dir_all(&marked).expect("the marked root");
        effigy_marker(&marked, 0xFFFF_FFFE, false, None);
        age_tree(&marked, 20);
        let now = sweep_now(0, 0);
        let record = harness.fixture.record.clone();
        let row_for = |name: &str, family: UnitFamily| super::PlanRow {
            name: name.to_owned(),
            scope: super::UnitScope::Project {
                name: "alpha".to_owned(),
            },
            family,
            path: harness.project("alpha").join(name),
            kind: crate::unit::UnitKind::Directory,
            device: 0,
            inode: 0,
            marker_file: None,
            marker_digest: String::new(),
            concluded_ms: None,
            created_ms: None,
            last_activity_ms: 0,
            grant: family.grant().to_owned(),
            reason: "the test's row".to_owned(),
        };
        // The identity a row records is what the authorization compares, so the
        // rows are filled from a first successful assessment below.
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let mut claimed = record.clone();
        claimed.scratch_grants.claims.push(ScratchClaim {
            scope: ClaimScope::Project {
                name: "alpha".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: idle,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        });
        let observed_row =
            |record: &crate::registry::VolumeRecord, name: &str, family: UnitFamily| {
                let mut row = row_for(name, family);
                let found = super::reassess_under(
                    &platform,
                    &facts,
                    &mounts,
                    &observer,
                    &harness.fixture.state,
                    record,
                    &row.scope,
                    name,
                    now,
                )
                .expect("the unit reassesses")
                .unwrap_or_else(|| panic!("`{name}` is eligible under the record"));
                row.device = found.observed.device.get();
                row.inode = found.observed.inode;
                row.marker_digest = found.observed.marker_digest;
                row
            };
        let unmarked = observed_row(&claimed, "leftover", UnitFamily::Unmarked);
        assert!(
            super::authorize_under(
                &platform,
                &facts,
                &mounts,
                &observer,
                &harness.fixture.state,
                &claimed,
                claimed.generation,
                &unmarked,
                now,
            )
            .is_ok(),
            "the claim's own idle age authorizes the fifteen-day unit"
        );

        // A stricter replacement claim over the same scope (thirty days, same
        // unit shape) no longer makes the unit eligible: the locked window
        // re-evaluates the policy, it does not just check that a claim exists.
        let mut stricter = claimed.clone();
        stricter.scratch_grants.claims[0].unmarked_idle_seconds = 30 * 24 * 60 * 60;
        let error = super::authorize_under(
            &platform,
            &facts,
            &mounts,
            &observer,
            &harness.fixture.state,
            &stricter,
            stricter.generation,
            &unmarked,
            now,
        )
        .expect_err("the stricter claim refuses the older unit");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(error.message().contains("no longer eligible"), "{error}");

        // A withdrawn claim is the same refusal.
        let mut withdrawn = claimed.clone();
        withdrawn.scratch_grants.claims.clear();
        assert_eq!(
            super::authorize_under(
                &platform,
                &facts,
                &mounts,
                &observer,
                &harness.fixture.state,
                &withdrawn,
                withdrawn.generation,
                &unmarked,
                now,
            )
            .expect_err("a withdrawn claim refuses")
            .kind(),
            ErrorKind::RefusedBoundary
        );

        // Marker precedence: the project claim never covers the marked family, so
        // the Effigy root needs the family's own delegation even though the
        // claim is current.
        let mut delegated = claimed.clone();
        delegated
            .scratch_grants
            .delegations
            .push(ScratchDelegation {
                family: MarkerFamily::EffigyScratch,
                by: "family owner".to_owned(),
                declared_ms: state::now_ms(),
            });
        let mut marked_row = observed_row(&delegated, "marked-run", UnitFamily::EffigyScratch);
        marked_row.marker_file = Some(super::EFFIGY_MARKER_FILE.to_owned());
        let error = super::authorize_under(
            &platform,
            &facts,
            &mounts,
            &observer,
            &harness.fixture.state,
            &claimed,
            claimed.generation,
            &marked_row,
            now,
        )
        .expect_err("a project claim never covers a marked family");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(
            super::authorize_under(
                &platform,
                &facts,
                &mounts,
                &observer,
                &harness.fixture.state,
                &delegated,
                delegated.generation,
                &marked_row,
                now,
            )
            .is_ok(),
            "the delegation is the marker family's authority"
        );

        let mut wrong_kind = unmarked.clone();
        wrong_kind.kind = crate::unit::UnitKind::File;
        assert_eq!(
            super::authorize_under(
                &platform,
                &facts,
                &mounts,
                &observer,
                &harness.fixture.state,
                &claimed,
                claimed.generation,
                &wrong_kind,
                now
            )
            .expect_err("a changed unit kind refuses")
            .kind(),
            ErrorKind::RefusedBoundary
        );

        // An identity that is not the plan's is refused under a current policy.
        let mut moved = unmarked.clone();
        moved.inode = moved.inode.saturating_add(1);
        assert_eq!(
            super::authorize_under(
                &platform,
                &facts,
                &mounts,
                &observer,
                &harness.fixture.state,
                &claimed,
                claimed.generation,
                &moved,
                now,
            )
            .expect_err("a changed identity refuses")
            .kind(),
            ErrorKind::RefusedBoundary
        );
    }

    #[test]
    fn a_deletion_refuses_a_withdrawn_grant_inside_its_locked_window() {
        let harness = Harness::new("scratch-auth-window");
        let root = harness.alloc("project-a", "owner");
        let (device, inode) = identity(&root);
        let record = &harness.fixture.record;
        let name = root.file_name().unwrap().to_string_lossy().to_string();
        let declaration = super::scratch_declaration(record, &scratch_folder(record));
        let marker = std::fs::read(root.join(area::ROOT_MARKER_FILE)).expect("marker");
        let unit = crate::unit::DeletionUnit {
            volume_uuid: harness.fixture.uuid.clone(),
            root_dir: record.root_dir.clone(),
            folder: scratch_folder(record),
            parent: "project-a".to_owned(),
            name: name.clone(),
            kind: crate::unit::UnitKind::Directory,
            device: crate::os::DeviceId::new(device),
            inode,
            marker: Some(crate::unit::UnitMarker::new(
                crate::unit::MarkerFamily::Barge,
                crate::digest::sha256_hex(&marker),
            )),
            grant: crate::unit::UnitGrant::native_root(&name),
            plan_digest: "ab".repeat(32),
        };
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let refuse = |_record: &crate::registry::VolumeRecord| {
            Err(crate::error::Error::new(
                ErrorKind::RefusedBoundary,
                "grant withdrawn",
            ))
        };
        let bounds = DeleteBounds {
            retry_pause: std::time::Duration::ZERO,
            ..DeleteBounds::default()
        };
        let error = crate::delete::delete_unit_authorized(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &declaration,
            None,
            &unit,
            bounds,
            Some(&refuse),
            &crate::state::IntentEvidence::none(),
        )
        .expect_err("withdrawn grant refuses");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(root.exists());
        assert!(state::read_intents(&harness.fixture.state, 64)
            .unwrap()
            .pending()
            .next()
            .is_none());
        area::release(
            &harness.fixture.state,
            &harness.fixture.observer(),
            &name,
            "owner",
        )
        .expect("release");
        let current = |_record: &crate::registry::VolumeRecord| Ok(());
        let report = crate::delete::delete_unit_authorized(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &declaration,
            None,
            &unit,
            bounds,
            Some(&current),
            &crate::state::IntentEvidence::none(),
        )
        .expect("delete reports");
        assert!(report.is_deleted(), "{report:?}");
        assert!(!root.exists());
    }

    #[test]
    fn concurrent_grant_writes_keep_both_declarations() {
        let harness = Harness::new("scratch-grant-concurrent");
        let state_paths = harness.fixture.state.clone();
        let uuid = harness.fixture.uuid.clone();
        // Hold the registry lock so both writers are inside their read-then-write
        // window when the other publishes: the interleaving that used to lose one
        // declaration (and could restore a withdrawn one).
        let mut locks = state::LockSet::new(state_paths.clone());
        let held = locks.lock_registry().expect("the registry lock");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for family in [MarkerFamily::OxideScratch, MarkerFamily::EffigyScratch] {
                let state_paths = state_paths.clone();
                let uuid = uuid.clone();
                let started = started_tx.clone();
                handles.push(scope.spawn(move || {
                    started.send(()).expect("the writer starts");
                    declare_delegation(
                        &state_paths,
                        &uuid,
                        family,
                        "operator",
                        state::now_ms(),
                        false,
                    )
                }));
            }
            drop(started_tx);
            for _ in 0..2 {
                started_rx.recv().expect("both writers started");
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
            handles
                .into_iter()
                .map(|handle| handle.join().expect("the writer joins"))
                .collect::<Vec<_>>()
        });
        for result in results {
            result.expect("each declaration is recorded");
        }
        let report =
            super::grants(&harness.fixture.state, &harness.fixture.uuid).expect("the grants read");
        assert_eq!(
            report.grants.delegations.len(),
            2,
            "one declaration was lost: {:?}",
            report.grants.delegations
        );
    }

    #[test]
    fn a_grant_write_waits_for_the_deletion_window() {
        let harness = Harness::new("scratch-grant-window");
        let state_paths = harness.fixture.state.clone();
        let uuid = harness.fixture.uuid.clone();
        let mut locks = state::LockSet::new(state_paths.clone());
        let held = locks.lock_volume(&uuid).expect("the volume lock");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let state_paths = state_paths.clone();
            let uuid = uuid.clone();
            let handle = scope.spawn(move || {
                let result = declare_delegation(
                    &state_paths,
                    &uuid,
                    MarkerFamily::OxideScratch,
                    "operator",
                    state::now_ms(),
                    false,
                );
                done_tx.send(()).expect("the writer finishes");
                result
            });
            // The writer must still be waiting: the volume lock is the deletion's
            // authorization window, and a grant change may not interleave it.
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_millis(200))
                    .is_err(),
                "the grant write did not wait for the volume lock"
            );
            drop(held);
            handle
                .join()
                .expect("the writer joins")
                .expect("the delegation is recorded");
        });
        let report =
            super::grants(&harness.fixture.state, &harness.fixture.uuid).expect("the grants read");
        assert_eq!(report.grants.delegations.len(), 1);
    }

    #[test]
    fn a_retained_or_invalid_conclusion_is_never_eligible() {
        let harness = Harness::new("scratch-conclusion-outcome");
        let retained = harness.alloc("project-a", "owner");
        let closed = harness.alloc("project-a", "owner");
        harness.conclude(
            &closed,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        harness.conclude(
            &retained,
            "owner",
            ConclusionOutcome::Closed,
            ConclusionClosure::Positive,
            KeptReason::Evidence,
            NestedScope::Exclusive,
        );
        // Rewrite one conclusion as if it had been retained, which the write path
        // now refuses: the reader must still never make it eligible.
        let marker_path = retained.join(area::ROOT_MARKER_FILE);
        let mut marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker_path).expect("the marker"))
                .expect("the marker parses");
        marker["conclusion"]["outcome"] = serde_json::json!("retained");
        marker["conclusion"]["concluded_ms"] = serde_json::json!(state::now_ms() - 30 * DAY_MS);
        write_marker(
            &marker_path,
            &serde_json::to_vec(&marker).expect("the marker encodes"),
        );
        let days = NATIVE_AGE_SECONDS / (24 * 60 * 60) + 1;
        let plan = harness.plan(days);
        let names = harness.candidate_names(&plan);
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(
            !names.contains(&retained.file_name().unwrap().to_string_lossy().to_string()),
            "the retained run is not a candidate: {names:?}"
        );
        let kept = plan
            .plan
            .kept
            .rows
            .iter()
            .find(|row| row.path.as_ref().is_some_and(|path| path == &retained))
            .expect("the retained run is reported kept");
        assert!(kept.reason.contains("retained"), "{}", kept.reason);

        // The write path refuses the same contradiction.
        let error = conclude(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &ConcludeRequest {
                volume_uuid: &harness.fixture.uuid,
                root_id: closed.file_name().unwrap().to_str().expect("a name"),
                holder: "owner",
                project: Some("project-a"),
                outcome: ConclusionOutcome::Retained,
                closure: ConclusionClosure::Positive,
                kept_reason: KeptReason::Evidence,
                nested_scope: NestedScope::Exclusive,
            },
        )
        .expect_err("a retained run cannot claim positive closure");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("retained"));
    }

    #[test]
    fn an_oxide_marker_with_invalid_or_future_facts_is_kept() {
        let harness = Harness::new("scratch-oxide-facts");
        harness.delegate(MarkerFamily::OxideScratch);
        let now = state::now_ms();
        let concluded = now - 30 * DAY_MS;
        let root = harness.project("oxide").join("oxide-proof.facts");
        std::fs::create_dir_all(&root).expect("the root");
        let (device, inode) = identity(&root);
        let write = |overrides: serde_json::Value| {
            let mut value = serde_json::json!({
                "version": 1,
                "run_id": "0123456789abcdef0123456789abcdef",
                "root": root.display().to_string(),
                "owner_uid": crate::os::platform().effective_uid().expect("the uid"),
                "device": device,
                "inode": inode,
                "created_unix": (concluded as f64) / 1000.0 - 60.0,
                "concluded_unix": (concluded as f64) / 1000.0,
                "outcome": "closed",
                "closure": "positive",
                "kept_reason": "evidence",
                "nested_scope": "exclusive",
            });
            for (key, replacement) in overrides.as_object().expect("an object").iter() {
                value[key] = replacement.clone();
            }
            write_marker(
                &root.join(super::OXIDE_MARKER_FILE),
                &serde_json::to_vec(&value).expect("the marker encodes"),
            );
        };
        let plan_at_now = || harness.plan_at(now + 3_600_000);

        // The valid marker is the control: it is eligible.
        write(serde_json::json!({}));
        let valid = plan_at_now();
        assert!(
            harness
                .candidate_names(&valid)
                .contains(&"oxide-proof.facts".to_owned()),
            "{:?}",
            valid.plan.candidates.rows
        );

        // A retained outcome, an invalid run id, a future creation and an
        // unreadable creation are each kept, and none is ever eligible.
        let cases = [
            (
                serde_json::json!({ "outcome": "retained" }),
                "never expirable",
            ),
            (serde_json::json!({ "run_id": "not-a-run-id" }), "32-hex"),
            (
                serde_json::json!({ "created_unix": (now as f64) / 1000.0 + 86_400.0 }),
                "future",
            ),
            (
                serde_json::json!({ "created_unix": "not a number" }),
                "not usable",
            ),
        ];
        for (overrides, expected) in cases {
            write(overrides);
            let plan = plan_at_now();
            let kept = plan
                .plan
                .kept
                .rows
                .iter()
                .find(|row| row.path.as_ref().is_some_and(|path| path == &root))
                .unwrap_or_else(|| panic!("the invalid marker is reported kept: {expected}"));
            assert!(
                kept.reason.contains(expected),
                "expected `{expected}` in `{}`",
                kept.reason
            );
            assert!(
                !harness
                    .candidate_names(&plan)
                    .contains(&"oxide-proof.facts".to_owned()),
                "an invalid oxide marker was eligible: {}",
                kept.reason
            );
        }
    }

    #[test]
    fn a_quarantined_file_leaf_replays_to_settlement() {
        let harness = Harness::new("scratch-file-replay");
        std::fs::create_dir_all(harness.scratch()).expect("the scratch folder");
        harness.adhoc_claim(60);
        let record = harness.fixture.record.clone();
        for (index, phase) in [IntentPhase::Renamed, IntentPhase::MarkerLast]
            .into_iter()
            .enumerate()
        {
            let name = format!("val.REPLAY{index}");
            let file = harness.scratch().join(&name);
            std::fs::write(&file, b"payload").expect("the ad-hoc file");
            let (device, inode) = identity(&file);
            let intent_id = format!("{:032x}", index + 11);
            let quarantine_name = format!("{}{intent_id}", state::QUARANTINE_PREFIX);
            std::fs::rename(&file, harness.scratch().join(&quarantine_name))
                .expect("the file is quarantined");
            let now = state::now_ms();
            let intent = state::IntentRecord {
                schema: crate::contract::INTENT_SCHEMA.to_owned(),
                intent_id: intent_id.clone(),
                volume_uuid: harness.fixture.uuid.clone(),
                mount_hint: harness.fixture.root.clone(),
                root_dir: record.root_dir.clone(),
                folder: scratch_folder(&record),
                unit: state::IntentUnit {
                    parent: String::new(),
                    name: name.clone(),
                    kind: crate::unit::UnitKind::File,
                    device,
                    inode,
                    marker: None,
                    capture_name: quarantine_name.clone(),
                    grant: crate::unit::UnitGrant::claim("top_level_adhoc"),
                },
                captures: Vec::new(),
                foreign_captures: Vec::new(),
                identity: None,
                absence: None,
                plan_digest: "ab".repeat(32),
                phase: IntentPhase::Intent,
                generation: 1,
                recorded_ms: now,
                phase_ms: now,
            };
            let mut locks = state::LockSet::new(harness.fixture.state.clone());
            {
                let _volume = locks
                    .lock_volume(&harness.fixture.uuid)
                    .expect("the volume lock");
                let _root = locks
                    .lock_unit(&state::unit_lock_key(
                        &intent.volume_uuid,
                        &intent.unit.parent,
                        &intent.unit.name,
                    ))
                    .expect("the unit lock");
                let write = locks.begin_intent_write().expect("the intent step");
                state::publish_intent(&write, &harness.fixture.state, &intent)
                    .expect("the intent publishes");
                state::advance_intent(&write, &harness.fixture.state, &intent, phase)
                    .expect("the phase advances");
            }
            drop(locks);

            let observer = harness.fixture.observer();
            let facts = harness.fixture.facts();
            let platform = harness.fixture.platform();
            let report = crate::delete::replay_pending(
                &harness.fixture.state,
                &platform,
                &facts,
                &observer,
                DeleteBounds {
                    retry_pause: std::time::Duration::ZERO,
                    ..DeleteBounds::default()
                },
                None,
            )
            .expect("the replay reports");
            assert!(
                report.pending.is_empty(),
                "a file leaf at {phase:?} stayed pending: {:?}",
                report.pending
            );
            assert_eq!(report.settled + report.resumed, 1, "{report:?}");
            assert!(
                !harness.scratch().join(&quarantine_name).exists(),
                "the verified file leaf was settled"
            );
            let intents = state::read_intents(&harness.fixture.state, 64).expect("the journal");
            assert!(intents.pending().next().is_none());
        }
    }

    #[test]
    fn a_pending_replay_makes_the_run_unsettled() {
        let replay = crate::delete::ReplayReport {
            examined: 1,
            settled: 0,
            rolled_back: 0,
            resumed: 0,
            remounted: 0,
            pending: vec![crate::delete::PendingRow {
                intent_id: "0123456789abcdef0123456789abcdef".to_owned(),
                unit_name: "leftover".to_owned(),
                unit_kind: "file".to_owned(),
                phase: crate::state::IntentPhase::Renamed,
                reason: "the area folder is gone but the marker-last phase was not recorded"
                    .to_owned(),
            }],
            entries_removed: 0,
        };
        let report = super::ExpireReport {
            volume_uuid: FIXTURE_UUID.to_owned(),
            volume_present: true,
            preview: false,
            plan_digest: "ab".repeat(32),
            plan: super::SweepPlan {
                schema: super::PLAN_SCHEMA.to_owned(),
                volume_uuid: harness_uuid(),
                mount_point: PathBuf::from("/fixture"),
                root_dir: String::new(),
                folder: "scratch".to_owned(),
                policy: SweepPolicy::design(),
                candidates: crate::contract::RowSet::empty(),
                kept: crate::contract::RowSet::empty(),
                examined: 0,
                truncated: false,
            },
            deleted: crate::contract::RowSet::empty(),
            pending: crate::contract::RowSet::empty(),
            skipped: crate::contract::RowSet::empty(),
            replay: Some(replay),
        };
        assert!(
            report.has_pending(),
            "a run with no new pending row but a pending replay must exit unsettled"
        );
    }

    fn harness_uuid() -> crate::os::VolumeUuid {
        crate::os::VolumeUuid::parse(FIXTURE_UUID).expect("the fixture UUID")
    }

    #[test]
    fn the_examination_budget_is_spent_inside_a_claimed_unit() {
        let harness = Harness::new("scratch-unit-budget");
        let unit = harness.project("alpha").join("leftover");
        std::fs::create_dir_all(&unit).expect("the unit");
        for index in 0..20 {
            std::fs::write(unit.join(format!("file-{index:02}.txt")), b"bytes").expect("a payload");
        }
        age_tree(&unit, 20);
        harness.claim("alpha", 60);
        let observer = harness.fixture.observer();
        let facts = harness.fixture.facts();
        let mounts = harness.fixture.mounts();
        let platform = harness.fixture.platform();
        let uuid = harness.fixture.uuid.clone();
        let request = SweepRequest {
            volume_uuid: &uuid,
            preview: true,
            plan_digest: None,
            bounds: SweepBounds {
                examine: 6,
                timeout: super::SWEEP_TIMEOUT,
            },
            delete_bounds: DeleteBounds::default(),
            now_ms: sweep_now(1, 0),
        };
        let report = sweep(
            &harness.fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect("the bounded preview reports")
        .report;
        assert!(report.plan.truncated, "the bound is reported");
        assert!(
            report.plan.examined <= 6,
            "the unit was examined past the bound: {}",
            report.plan.examined
        );
        assert!(
            report
                .plan
                .candidates
                .rows
                .iter()
                .all(|row| row.name != "leftover"),
            "an unexamined unit was made eligible: {:?}",
            report.plan.candidates.rows
        );
        let kept = report
            .plan
            .kept
            .rows
            .iter()
            .find(|row| row.name == "leftover")
            .expect("the unit is reported kept");
        assert!(kept.reason.contains("budget"), "{}", kept.reason);
    }

    // ---- eight-character project names (O-005 architect ruling) ------------

    #[test]
    fn an_eight_character_project_claim_is_accepted_and_expires_its_children() {
        let harness = Harness::new("scratch-eight-character-project");
        // The old shape-only rule refused every eight-character name because it
        // had the shape of a run folder. The claim, not the name, establishes the
        // project, so this is the counterfactual: the same name now has a claim.
        assert!(super::is_run_name("underlay"));
        let child = harness.project("underlay").join("old-child");
        std::fs::create_dir_all(&child).expect("the unit");
        std::fs::write(child.join("payload.txt"), b"bytes").expect("a payload");
        age_tree(&child, 20);
        harness.claim("underlay", 14 * 24 * 60 * 60);
        let now = state::now_ms() + 3_600_000;
        let plan = harness.plan_at(now);
        assert_eq!(
            plan.plan.candidates.count, 1,
            "{:?} {:?}",
            plan.plan.candidates.rows, plan.plan.kept.rows
        );
        assert_eq!(plan.plan.candidates.rows[0].name, "old-child");
        assert_eq!(
            plan.plan.candidates.rows[0].scope,
            super::UnitScope::Project {
                name: "underlay".to_owned()
            }
        );
        let real = harness.expire_at(now, &plan.plan_digest);
        assert_eq!(real.deleted.count, 1, "{:?}", real.skipped.rows);
        assert!(!child.exists(), "the claimed child was deleted");
    }

    #[test]
    fn a_marked_family_inside_a_claimed_eight_character_project_needs_its_delegation() {
        let harness = Harness::new("scratch-eight-character-marked");
        let marked = harness.project("underlay").join("marked-run");
        std::fs::create_dir_all(&marked).expect("the effigy root");
        effigy_marker(&marked, 0xFFFF_FFFE, false, None);
        age_tree(&marked, 20);
        harness.claim("underlay", 14 * 24 * 60 * 60);
        let now = state::now_ms() + 3_600_000;
        let plan = harness.plan_at(now);
        assert!(
            plan.plan.candidates.rows.is_empty(),
            "a marked family is never covered by a project claim: {:?}",
            plan.plan.candidates.rows
        );
        assert!(
            harness
                .kept_reason(&plan, "marked-run")
                .contains("no delegation"),
            "{:?}",
            plan.plan.kept.rows
        );
        harness.delegate(MarkerFamily::EffigyScratch);
        let granted = harness.plan_at(now);
        assert_eq!(
            granted.plan.candidates.count, 1,
            "{:?}",
            granted.plan.candidates.rows
        );
        assert_eq!(granted.plan.candidates.rows[0].name, "marked-run");
    }

    #[test]
    fn an_unclaimed_eight_character_name_survives_and_the_ad_hoc_claim_covers_it() {
        let harness = Harness::new("scratch-eight-character-unclaimed");
        let child = harness.project("underlay").join("old-child");
        std::fs::create_dir_all(&child).expect("the unit");
        age_tree(&harness.project("underlay"), 20);
        let now = state::now_ms() + 3_600_000;
        let bare = harness.plan_at(now);
        assert!(
            bare.plan.candidates.rows.is_empty(),
            "an unclaimed eight-character name is never a project: {:?}",
            bare.plan.candidates.rows
        );
        assert!(bare
            .plan
            .kept
            .rows
            .iter()
            .any(|row| row.name == "underlay" && row.reason.contains("eight-character")));
        let run = harness.scratch().join("underlay");
        assert!(
            !reaper_units(&harness.scratch(), now).contains(&run),
            "the host reaper does not delete this eight-character directory as a unit"
        );
        let no_claim = harness.expire_at(now, &bare.plan_digest);
        assert_eq!(no_claim.deleted.count, 0);
        assert!(
            run.is_dir(),
            "the real sweep keeps the unit without a claim"
        );
        // The ruled exception: a top-level claim reads the folder itself as one unit.
        harness.adhoc_claim(14 * 24 * 60 * 60);
        let conflicting = ScratchClaim {
            scope: ClaimScope::Project {
                name: "underlay".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 60,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        let error = declare_claim(
            &harness.fixture.state,
            &harness.fixture.uuid,
            &conflicting,
            false,
        )
        .expect_err("project and top-level claims cannot overlap");
        assert_eq!(error.kind(), ErrorKind::Usage);
        let adhoc = harness.plan_at(now);
        assert_eq!(adhoc.plan.candidates.count, 1);
        assert_eq!(adhoc.plan.candidates.rows[0].name, "underlay");
        assert_eq!(
            adhoc.plan.candidates.rows[0].scope,
            super::UnitScope::TopLevel
        );
        let real = harness.expire_at(now, &adhoc.plan_digest);
        assert_eq!(real.deleted.count, 1, "{:?}", real.skipped.rows);
        assert!(!harness.scratch().join("underlay").exists());
    }

    #[test]
    fn a_project_claim_and_a_top_level_claim_may_not_overlap_on_an_eight_character_name() {
        let harness = Harness::new("scratch-eight-character-overlap");
        harness.claim("underlay", 60);
        let adhoc = ScratchClaim {
            scope: ClaimScope::TopLevelAdhoc,
            unit: ClaimUnit::Entry,
            unmarked_idle_seconds: 60,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        let error = declare_claim(&harness.fixture.state, &harness.fixture.uuid, &adhoc, false)
            .expect_err("the overlapping ad-hoc claim is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(
            error.message().contains("may not cover the same name"),
            "{error}"
        );
        // A withdrawal is always allowed: it is how an operator resolves an
        // overlap, and withdrawing the project claim is what releases the name.
        let project = ScratchClaim {
            scope: ClaimScope::Project {
                name: "underlay".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 60,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        assert!(
            declare_claim(
                &harness.fixture.state,
                &harness.fixture.uuid,
                &project,
                true
            )
            .expect("the withdrawal is allowed")
            .changed
        );
        declare_claim(&harness.fixture.state, &harness.fixture.uuid, &adhoc, false)
            .expect("the ad-hoc claim is recorded once the other is withdrawn");
        // The other direction refuses the same way.
        let error = declare_claim(
            &harness.fixture.state,
            &harness.fixture.uuid,
            &project,
            false,
        )
        .expect_err("the overlapping project claim is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
        // A project claim for a name that is never an ad-hoc unit (a plain
        // project name) coexists with the ad-hoc claim: only an eight-character
        // name can be read both ways, and that is the overlap refused above.
        let plain = ScratchClaim {
            scope: ClaimScope::Project {
                name: "alpha".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 60,
            families_excluded: vec![],
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        declare_claim(&harness.fixture.state, &harness.fixture.uuid, &plain, false)
            .expect("a plain project claim is recorded beside the ad-hoc claim");
    }
}
