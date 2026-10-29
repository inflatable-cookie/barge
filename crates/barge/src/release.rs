//! The release gate: may this whole owned root be given up?
//!
//! Stage 4's one gate (design, "Releasability: the one gate"). A release answers
//! a different question from the stage 3 space prune — "is this root still
//! wanted?" rather than "does the volume need the room?" — and it is answered by
//! six conditions, each with its own evidence, each re-proved at an effect:
//!
//! 1. the volume is verified: APFS, not boot, not duplicated, mounted once;
//! 2. the root's durable identity verdict is `Verified`;
//! 3. custody is `Safe`;
//! 4. the area's declared `PrunePolicy` allows releasing it, the volume's
//!    durability is `expendable`, and its age from last activity is past the
//!    declared age;
//! 5. every reference naming the root, from a complete reference set, is not
//!    current;
//! 6. every reference whose own rule requires absence has an `Absent` proof of
//!    its consumer path, established now.
//!
//! This slice plans and never acts. `barge prune --plan` produces
//! `barge.prune.plan/1`, one area on one volume, where condition 1 is the plan's
//! own precondition and every row carries conditions 2 to 6 with their evidence
//! whether or not it is eligible — which is what makes one plan a diagnostic as
//! well as a permission. `barge.prune/1` is the command document
//! around it, exactly as `barge.maintain/1` wraps the maintain plan: the plan,
//! its SHA-256, the volume's state and the Oxide snapshot's own completeness.
//! The effect, its intent evidence and its replay are stage 4 F4.
//!
//! Three rules the module keeps:
//!
//! - **Unknown is never eligible.** A missing or invalid identity record, an
//!   unreadable hold, an incomplete reference set, an unprovable absence, a
//!   future modification time and a reached bound each keep the root.
//! - **One object never blocks another.** Every row is decided on its own
//!   evidence: another root's refusal, another root's unreadable marker or
//!   another root's unsettled custody leaves this row exactly as it would have
//!   been. Only a fact that is about the whole batch — an absent volume, an
//!   unreadable registry — is reported for every row it covers.
//! - **The plan is not a permission.** Its bytes and digest are the artifact a
//!   real run will present, and every predicate in them is evidence that must be
//!   re-established, because the path can reappear and the volume can be pulled
//!   between the two. It still takes the design's locks in the design's order —
//!   the registry observation lock and released, then the volume's shared lock,
//!   then one root's shared lock at a time — so a preview never reads a root or a
//!   volume a live run is mutating, and a lock it cannot take inside its own
//!   budget is a report rather than something waited out.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::area::{self, ActivityMode, AreaDocument, MarkerStateName, PrunePolicy};
use crate::contract::{RowSet, DOCUMENT_SIZE_CAP_BYTES, PRUNE_PLAN_SCHEMA};
use crate::custody::{self, CustodyBounds, PendingIntent};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::identity::{self, IdentityReason, IdentityVerdict};
use crate::os::{
    AbsenceVolume, AncestorExpected, DeviceId, DirOps, MountRoot, ProcessObserver, VolumeUuid,
};
use crate::prune::VolumeState;
use crate::reference::{self, BargeReferences, OxideSnapshot, ReferenceRow};
use crate::registry::{Availability, Durability, VolumeRecord};
use crate::state::{self, Deadline, StatePaths};
use crate::unit::{DeletionUnit, MarkerFamily, UnitGrant, UnitKind, UnitMarker};

/// How many references one root's row reports before it says the set was
/// truncated. A root with more is kept: an unread list is never a short one.
pub const REFERENCE_ROWS: usize = 64;

/// The most roots one plan examines (stage 4 design, "Bounded resources").
pub const PLAN_ROOTS: usize = 4096;

/// The verdict a proof that could not be established carries.
const ABSENCE_VERDICT_UNKNOWN: &str = "unknown";

/// One plan's bounds. The design's custody bounds are the ones a delete uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseBounds {
    /// The custody scan's bounds, which a plan shares with the effect.
    pub custody: CustodyBounds,
    /// How long one shared lock may wait for a live owner. It is the run's own
    /// deadline for a lock: a holder past it is refused, never waited out twice.
    pub lock_wait: Duration,
}

impl ReleaseBounds {
    /// The design's bounds: the custody scan's, and the state directory's own lock
    /// wait.
    pub fn design() -> Self {
        Self {
            custody: CustodyBounds::default(),
            lock_wait: state::LOCK_WAIT,
        }
    }
}

impl Default for ReleaseBounds {
    /// [`ReleaseBounds::design`].
    fn default() -> Self {
        Self::design()
    }
}

/// One area, as a plan names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRef {
    /// The consumer's namespace.
    pub namespace: String,
    /// The consumer's area name.
    pub area: String,
}

/// The identity evidence one row carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityEvidence {
    /// `verified`, `unknown` or `gone`.
    pub verdict: String,
    /// The reason code, when the verdict is not `verified`. It is one of the
    /// stage 4 design's codes, spelled in snake case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The inode the durable record holds, when the record was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    /// The device the record was written at. It is diagnostic only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_at_record: Option<u64>,
    /// The device the root was observed at in this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_live: Option<u64>,
    /// What the remount changed, which is never a refusal on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remount: Option<RemountEvidenceRow>,
    /// Whether the live mount differs from the recorded one. Also not a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_moved: Option<bool>,
    /// The SHA-256 of the marker bytes this plan read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_digest: Option<String>,
}

/// The remount evidence a verified identity reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemountEvidenceRow {
    /// Whether the device at the record differs from the live device.
    pub device_changed: bool,
}

/// The age evidence one row carries. It is evidence, never a permission: a real
/// run re-reads the root's activity before it acts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleEvidence {
    /// The newest activity the bounded scan found, when it could decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    /// `now - last_activity_ms`, from this plan's own clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<u64>,
    /// The age the declaration required, when it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_age_seconds: Option<u64>,
    /// Why the age could not be decided, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The bytes one root's scan counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BytesEvidence {
    /// The logical bytes the scan summed.
    pub logical: u64,
    /// The allocated bytes the scan summed.
    pub allocated: u64,
    /// Whether the scan reached the whole subtree inside its bounds.
    pub complete: bool,
}

/// One `barge.absence/1` proof, embedded in a plan row.
///
/// It is the proof's own field set, so a reader of the plan reads the same
/// document a diagnostic would print. It authorises nothing: the effect
/// re-establishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceEvidence {
    /// Always `barge.absence/1`.
    pub schema: String,
    /// The consumer path the proof is about.
    pub path: PathBuf,
    /// `absent`, `present` or `unknown`.
    pub verdict: String,
    /// Where the walk started.
    pub anchor: AbsenceAnchor,
    /// The nearest existing ancestor, when the walk reached one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<AbsenceAncestor>,
    /// The leaf, when it was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<AbsenceLeaf>,
    /// How many components the walk crossed.
    #[serde(default)]
    pub components_walked: usize,
    /// When the proof was established.
    #[serde(default)]
    pub proved_ms: u64,
    /// The reason code, when the verdict is `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where one absence proof walked from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceAnchor {
    /// `root` or `registered`.
    pub kind: String,
    /// The anchor's path.
    pub path: PathBuf,
}

/// The nearest existing ancestor one absence proof verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceAncestor {
    /// The ancestor's path.
    pub path: PathBuf,
    /// Its inode.
    pub inode: u64,
    /// Its device.
    pub device: u64,
    /// The volume it was proved on.
    pub volume: AbsenceVolumeRow,
}

/// The volume an ancestor was proved on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceVolumeRow {
    /// `boot_data` or `registered`.
    pub kind: String,
    /// The APFS volume UUID.
    pub volume_uuid: String,
    /// The filesystem the platform reported.
    pub file_system: String,
}

/// The leaf one absence proof decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceLeaf {
    /// The leaf's name.
    pub name: String,
    /// What the walk found: `enoent`.
    pub result: String,
}

/// One root's row in the release plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrunePlanRow {
    /// The root's id, which is the marker's and the directory's name.
    pub root_id: String,
    /// The path below the verified mount root, which is what survives a
    /// remount.
    pub mount_relative_path: String,
    /// Condition 2's evidence.
    pub identity: IdentityEvidence,
    /// Condition 3's verdict: `safe`, `unsafe` or `unknown`.
    pub custody: String,
    /// Condition 3's reason, when it is not `safe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custody_reason: Option<String>,
    /// Condition 4's evidence.
    pub idle: IdleEvidence,
    /// Condition 5's evidence: one row per reference that names the root.
    #[serde(default)]
    pub references: Vec<ReferenceRow>,
    /// Whether every source that could name the root was read completely.
    pub references_complete: bool,
    /// Condition 6's evidence, when a reference requires an absence proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absence: Option<AbsenceEvidence>,
    /// The bytes this plan's scan counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<BytesEvidence>,
    /// Whether all six conditions hold.
    pub eligible: bool,
    /// The first condition that keeps the root, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    /// The first reason the root is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The `barge.prune.plan/1` body: one bounded canonical plan and nothing else.
///
/// The gate's first condition — the volume is verified, APFS, not boot and
/// mounted once — is the plan's own precondition rather than a row field: a plan
/// exists only for a volume that verified, and the command document's
/// `volume.state` records that it did. An attached volume that fails to verify is
/// a refused command, so no row can claim condition 1 on a volume it did not
/// establish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrunePlan {
    /// Always `barge.prune.plan/1`.
    pub schema: String,
    /// The one area this plan releases from.
    pub area: AreaRef,
    /// The one volume its roots are on.
    pub volume_uuid: String,
    /// One row per root examined, in listing order.
    pub rows: RowSet<PrunePlanRow>,
    /// How many rows are eligible.
    pub eligible_count: usize,
    /// How many roots the listing examined, including any it could not report.
    pub examined_count: usize,
    /// Whether a listing bound cut the plan before the area was exhausted.
    pub truncated: bool,
}

impl PrunePlan {
    /// The roots this plan would release.
    pub fn eligible(&self) -> impl Iterator<Item = &PrunePlanRow> {
        self.rows.rows.iter().filter(|row| row.eligible)
    }
}

/// One volume's state in the plan's command document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVolume {
    /// The registered UUID.
    pub volume_uuid: String,
    /// How the registry says the volume is attached.
    pub availability: Availability,
    /// What the data on it is.
    pub durability: Durability,
    /// Whether it was attached when the plan ran.
    pub state: VolumeState,
    /// Whether this volume needs a human: a permanent volume that is away, or a
    /// durability an operator has not declared.
    pub loud: bool,
    /// Why the volume has no rows, when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What the plan's Oxide reference snapshot read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OxideEvidence {
    /// The control directory the snapshot read, when one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<PathBuf>,
    /// Whether every Oxide record was read completely.
    pub complete: bool,
    /// Why it was not, when it was not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The `barge.prune/1` command document: the plan, its digest, and what the run
/// observed around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruneReport {
    /// Whether this run only planned. It always does in this slice.
    pub preview: bool,
    /// The SHA-256 of the plan's canonical bytes.
    pub plan_digest: String,
    /// When the plan was built.
    pub planned_ms: u64,
    /// The stable plan.
    pub plan: PrunePlan,
    /// The volume's state.
    pub volume: PlanVolume,
    /// What the reference snapshot read.
    pub oxide: OxideEvidence,
}

impl PruneReport {
    /// The exit code the finished document forces.
    ///
    /// `3` is the design's "unknown state that needs a human": a permanent
    /// volume that is away, a volume whose durability an operator has not
    /// declared, or a reference set that could not be read completely. `0` is a
    /// plan whose every refusal is an ordinary "keep this root".
    pub fn needs_human(&self) -> bool {
        self.volume.loud || !self.oxide.complete
    }
}

/// What one caller asks `barge prune --plan` for.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseRequest<'a> {
    /// The area's namespace.
    pub namespace: &'a str,
    /// The area's name.
    pub area: &'a str,
    /// The one volume to plan against.
    pub volume_uuid: &'a VolumeUuid,
    /// This run's clock, so a test can age a root without waiting.
    pub now_ms: u64,
    /// The run's bounds.
    pub bounds: ReleaseBounds,
}

/// Plan the release of one area's roots on one volume. Nothing is changed.
///
/// The six conditions are decided per root and reported with their evidence.
/// An absent volume, an undeclared area, an unreadable registry or a volume
/// whose durability never permits pruning produces no eligible row and says why;
/// it is never read as "nothing to do".
#[allow(clippy::too_many_arguments)]
pub fn plan(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    account: &dyn crate::os::AccountFacts,
    request: &ReleaseRequest<'_>,
) -> Result<PruneReport> {
    let records = read_registry(state_paths)?;
    let Some(record) = records
        .iter()
        .find(|record| record.volume_uuid == *request.volume_uuid)
    else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "volume {} is not registered, so nothing can be planned for it",
                request.volume_uuid
            ),
        )
        .with_detail("volume_uuid", request.volume_uuid.as_str().into()));
    };
    let Some(document) = area::show(state_paths, request.namespace, request.area)? else {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "the area {}/{} is not declared, so there is no release policy to apply; nothing was planned",
                request.namespace, request.area
            ),
        ));
    };
    let declaration = document.declaration();

    let mut volume = PlanVolume {
        volume_uuid: record.volume_uuid.as_str().to_owned(),
        availability: record.availability,
        durability: record.durability,
        state: VolumeState::Present,
        loud: false,
        detail: None,
    };
    let area_ref = AreaRef {
        namespace: document.namespace.clone(),
        area: document.area.clone(),
    };

    // Condition 1, at the volume's own level. An attached volume that does not
    // verify is a refusal of the command, not a plan: nothing about its roots
    // can be observed, and reporting them as merely "kept" would hide a
    // mismatched volume.
    if let Err(error) = area::VolumeRoot::open(dirs, facts, mounts, record) {
        if error.kind() != ErrorKind::VolumeAbsent {
            return Err(error);
        }
        volume.state = VolumeState::Absent;
        volume.loud = record.availability == Availability::Permanent;
        volume.detail = Some(if volume.loud {
            format!(
                "permanent volume {} is not attached; its roots were not observed and its mount point was not recreated",
                record.volume_uuid
            )
        } else {
            format!(
                "ephemeral volume {} is not attached; it was skipped",
                record.volume_uuid
            )
        });
        return finish(
            volume,
            area_ref,
            request,
            PlanRows::empty(),
            OxideSnapshot::none(),
        );
    }
    let durability = match record.declared_durability() {
        Ok(durability) => durability,
        Err(error) => {
            volume.loud = true;
            volume.detail = Some(format!(
                "volume {} has no declared durability, so nothing on it can be released: {error}",
                record.volume_uuid
            ));
            return finish(
                volume,
                area_ref,
                request,
                PlanRows::empty(),
                OxideSnapshot::none(),
            );
        }
    };
    if durability != Durability::Expendable {
        volume.detail = Some(format!(
            "volume {} is declared `{}`, so no root on it is expendable and none can be released",
            record.volume_uuid,
            durability_name(durability)
        ));
        return finish(
            volume,
            area_ref,
            request,
            PlanRows::empty(),
            OxideSnapshot::none(),
        );
    }

    // The lock order the design fixes: the registry's observation lock was taken
    // and released by `read_registry`, and this is the volume's shared lock held
    // for the rest of the pass. A real prune of this volume takes the same file
    // exclusively, so a preview never reads a volume mid-prune; a holder that
    // outlives this run's lock budget refuses the preview and changes nothing.
    let mut locks = state::LockSet::with_deadline(
        state_paths.clone(),
        Deadline::after(request.bounds.lock_wait),
    );
    let volume_lock = match locks.observe_volume(&record.volume_uuid) {
        Ok(lock) => lock,
        Err(error) => {
            let path = state_paths.volume_lock_path(&record.volume_uuid);
            return Err(Error::new(
                ErrorKind::Locked,
                format!(
                    "volume {}'s lock could not be observed, so this volume's release plan was refused: {}; nothing was changed",
                    record.volume_uuid,
                    error.message()
                ),
            )
            .with_detail("volume_uuid", record.volume_uuid.as_str().into())
            .with_detail("lock", path.display().to_string().into()));
        }
    };
    let resolved = area::resolve(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &declaration,
        Some(&document),
    )?;
    // The age decision is the stage 4 rule: a bounded scan of each root's
    // subtree, never the marker's creation time alone.
    let listing = area::list_roots_with(
        dirs,
        observer,
        state_paths,
        &resolved,
        PLAN_ROOTS,
        ActivityMode::Scan,
    )?;
    let placements = BargeReferences::read(state_paths);
    let oxide = OxideSnapshot::read(account);
    let pending = pending_intents(state_paths)?;
    let uid = observer.effective_uid()?;

    let mut rows = Vec::new();
    let truncated = listing.truncated;
    for root in &listing.roots {
        rows.push(decide_root(
            state_paths,
            dirs,
            facts,
            mounts,
            observer,
            record,
            &resolved,
            &document,
            &declaration,
            &placements,
            &oxide,
            &pending,
            uid,
            request,
            root,
        ));
    }
    // One root's shared lock at a time: every row took and released its own, so
    // the volume's observation lock is the only one still held here.
    drop(volume_lock);
    let examined = listing.examined;
    finish(
        volume,
        area_ref,
        request,
        PlanRows::with_counts(rows, examined, truncated),
        oxide,
    )
}

/// The rows and counts one plan assembles.
struct PlanRows {
    rows: Vec<PrunePlanRow>,
    examined: usize,
    truncated: bool,
}

impl PlanRows {
    /// A plan with no rows.
    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            examined: 0,
            truncated: false,
        }
    }

    /// A plan with the listing's own counts.
    fn with_counts(rows: Vec<PrunePlanRow>, examined: usize, truncated: bool) -> Self {
        Self {
            rows,
            examined,
            truncated,
        }
    }
}

/// Assemble the report, its plan and its digest.
fn finish(
    volume: PlanVolume,
    area_ref: AreaRef,
    request: &ReleaseRequest<'_>,
    rows: PlanRows,
    oxide: OxideSnapshot,
) -> Result<PruneReport> {
    let eligible_count = rows.rows.iter().filter(|row| row.eligible).count();
    let total = rows.rows.len();
    let plan = PrunePlan {
        schema: PRUNE_PLAN_SCHEMA.to_owned(),
        area: area_ref,
        volume_uuid: volume.volume_uuid.clone(),
        rows: RowSet::bounded(rows.rows, total, PLAN_ROOTS),
        eligible_count,
        examined_count: rows.examined,
        truncated: rows.truncated || plan_rows_truncated(total),
    };
    let plan_digest = sha256_hex(&canonical_plan_bytes(&plan)?);
    Ok(PruneReport {
        preview: true,
        plan_digest,
        planned_ms: request.now_ms,
        plan,
        oxide: OxideEvidence {
            control: oxide.control.clone(),
            complete: oxide.complete(),
            detail: oxide.detail.clone(),
        },
        volume,
    })
}

/// Whether the plan's own row cap cut the listing.
const fn plan_rows_truncated(total: usize) -> bool {
    total > PLAN_ROOTS
}

fn durability_name(durability: Durability) -> &'static str {
    match durability {
        Durability::Expendable => "expendable",
        Durability::Kept => "kept",
        Durability::Undeclared => "undeclared",
    }
}

/// Read the registry under the shared observation lock. Nothing is created.
fn read_registry(state_paths: &StatePaths) -> Result<Vec<VolumeRecord>> {
    let mut locks = state::LockSet::new(state_paths.clone());
    let _lock = locks.observe_registry()?;
    Ok(crate::registry::read(state_paths)?.volumes().to_vec())
}

/// The intents an earlier run left pending, which keep every unit they name.
fn pending_intents(state_paths: &StatePaths) -> Result<Vec<PendingIntent>> {
    let intents = state::read_intents(state_paths, crate::contract::INTENT_ROWS)?;
    if intents.truncated {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the intent journal is truncated, so no root's custody can be proved",
        ));
    }
    Ok(PendingIntent::pending(&intents.records))
}

/// Decide one root's row.
#[allow(clippy::too_many_arguments)]
fn decide_root(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    resolved: &area::ResolvedArea,
    document: &AreaDocument,
    declaration: &crate::area::AreaDeclaration,
    placements: &BargeReferences,
    oxide: &OxideSnapshot,
    pending: &[PendingIntent],
    uid: u32,
    request: &ReleaseRequest<'_>,
    root: &area::RootObservation,
) -> PrunePlanRow {
    let root_id = root.root_id.clone().unwrap_or_else(|| root.name.clone());
    let mount_relative_path = relative_to_mount(&root.configured_path, &resolved.mount_point);
    // One root's shared lock, held for this row alone and dropped before the next
    // root. A consumer's hold and a delete take this file exclusively, so a plan
    // never observes a root whose own lock a live run holds; a holder that
    // outlives the run's lock budget keeps this root only, and the rest of the
    // area still plans.
    let root_lock = if state::is_identifier(&root_id) {
        let mut locks = state::LockSet::with_deadline(
            state_paths.clone(),
            Deadline::after(request.bounds.lock_wait),
        );
        match locks.observe_root(&root_id) {
            Ok(lock) => lock,
            Err(error) => {
                return locked_row(
                    &root_id,
                    mount_relative_path,
                    root,
                    format!(
                        "the root's own lock could not be observed, so nothing about this root was read: {}",
                        error.message()
                    ),
                );
            }
        }
    } else {
        // A name that is not a Barge root id cannot be a root Barge owns, and a
        // lock file is never named from an unvalidated string. The row's own
        // identity evidence keeps it; there is no root lock to take.
        None
    };
    let stored = identity::read(state_paths, dirs, &root_id, uid)
        .ok()
        .flatten();
    let mount_relative_path = stored.as_ref().map_or(mount_relative_path, |record| {
        record.mount_relative_path.clone()
    });
    let mut blockers: Vec<(&'static str, String)> = Vec::new();

    // Condition 2: the durable identity.
    let identity = identity_evidence(
        state_paths,
        dirs,
        facts,
        mounts,
        uid,
        &root_id,
        root,
        stored.as_ref(),
    );
    if identity.verdict != "verified" {
        blockers.push((
            "identity",
            match &identity.reason {
                Some(reason) => format!("the root's identity is not verified: {reason}"),
                None => format!("the root's identity is {}", identity.verdict),
            },
        ));
    }

    // Condition 3: custody.
    let (custody, custody_reason) = match unit_for(record, resolved, root) {
        Some(unit) => {
            let verdict = custody::take_custody(
                dirs,
                facts,
                mounts,
                observer,
                state_paths,
                &unit,
                pending,
                request.bounds.custody,
            );
            let reason = verdict.reason().map(ToOwned::to_owned);
            (verdict.name().to_owned(), reason)
        }
        None => (
            "unknown".to_owned(),
            Some(
                "the entry is not an owned root, so no deletion unit can be built for it"
                    .to_owned(),
            ),
        ),
    };
    if custody != "safe" {
        blockers.push((
            "custody",
            match &custody_reason {
                Some(reason) => format!("custody is not safe: {reason}"),
                None => format!("custody is {custody}"),
            },
        ));
    }

    // Condition 4: policy, durability and age.
    let declared_age = match &declaration.prune {
        PrunePolicy::None => None,
        PrunePolicy::OldestIdleAfter { age_seconds } => Some(*age_seconds),
    };
    let scan = root.activity.clone().unwrap_or_else(|| area::RootActivity {
        last_activity_ms: None,
        logical_bytes: 0,
        allocated_bytes: 0,
        entries: 0,
        complete: false,
        reason: Some("the listing did not scan the root".to_owned()),
    });
    let idle =
        match (declared_age, scan.last_activity_ms) {
            (None, _) => IdleEvidence {
                last_activity_ms: scan.last_activity_ms,
                age_seconds: None,
                declared_age_seconds: None,
                reason: Some("the area declares no prune age, so no root is released".to_owned()),
            },
            (Some(declared), Some(last)) => IdleEvidence {
                last_activity_ms: Some(last),
                age_seconds: Some(request.now_ms.saturating_sub(last) / 1000),
                declared_age_seconds: Some(declared),
                reason: scan.reason.clone(),
            },
            (Some(declared), None) => IdleEvidence {
                last_activity_ms: None,
                age_seconds: None,
                declared_age_seconds: Some(declared),
                reason: Some(scan.reason.clone().unwrap_or_else(|| {
                    "the root's last activity could not be observed".to_owned()
                })),
            },
        };
    match declared_age {
        None => blockers.push((
            "policy",
            "the area declares no prune age, so its roots are never released".to_owned(),
        )),
        Some(declared) => match scan.last_activity_ms {
            None => blockers.push((
                "policy",
                format!(
                    "the root's age is unknown, so it is not past the declared {declared} s: {}",
                    idle.reason.as_deref().unwrap_or("no reason reported")
                ),
            )),
            Some(last) => {
                let age_ms = request.now_ms.saturating_sub(last);
                if age_ms < declared.saturating_mul(1000) {
                    blockers.push((
                        "policy",
                        format!("idle for less than the declared {declared} s"),
                    ));
                }
            }
        },
    }

    // Condition 5 and 6: the references, then the absence proofs they need.
    let mut references = placements.references_for(
        state_paths,
        observer,
        document,
        declared_age.is_some(),
        &root_id,
        &root.configured_path,
    );
    let (marker, marker_unknown) = match dirs.open_path(&root.configured_path) {
        Ok(handle) => match reference::read_oxide_marker(dirs, &handle) {
            Ok(Some(marker)) => (Some(marker), None),
            Ok(None) => (None, None),
            Err(reason) => (None, Some(reason)),
        },
        Err(error) => (None, Some(format!("the root cannot be opened: {error}"))),
    };
    references.absorb(oxide.references_for(
        &root.configured_path,
        marker.as_ref(),
        marker_unknown.as_deref(),
    ));
    let references_truncated = references.rows.len() > REFERENCE_ROWS;
    let mut reference_rows = references.rows.clone();
    if references_truncated {
        reference_rows.truncate(REFERENCE_ROWS);
    }
    if !references.complete {
        blockers.push((
            "references",
            format!(
                "the reference set is not complete: {}",
                references
                    .detail
                    .as_deref()
                    .unwrap_or("a source could not be read")
            ),
        ));
    }
    if references_truncated {
        blockers.push((
            "references",
            format!("the root carries more than the {REFERENCE_ROWS} references a plan reports"),
        ));
    }
    if references.any_current() {
        let sources: Vec<&str> = references
            .rows
            .iter()
            .filter(|row| row.current)
            .map(|row| row.source.as_str())
            .collect();
        blockers.push((
            "references",
            format!("a reference still names this root: {}", sources.join(", ")),
        ));
    }
    let (absence, absence_reason) = prove_references(dirs, facts, &references.absence);
    if let Some(reason) = absence_reason {
        blockers.push(("absence", reason));
    }

    // The row is decided; the root's own lock is released before the next root,
    // so two roots are never held at once.
    drop(root_lock);
    let eligible = blockers.is_empty();
    let (blocked_by, reason) = match blockers.into_iter().next() {
        Some((condition, reason)) => (Some(condition.to_owned()), Some(reason)),
        None => (None, None),
    };
    PrunePlanRow {
        root_id,
        mount_relative_path,
        identity,
        custody,
        custody_reason,
        idle,
        references: reference_rows,
        references_complete: references.complete && !references_truncated,
        absence,
        bytes: Some(BytesEvidence {
            logical: scan.logical_bytes,
            allocated: scan.allocated_bytes,
            complete: scan.complete,
        }),
        eligible,
        blocked_by,
        reason,
    }
}

/// The identity evidence for one root, from the durable record and the walk.
#[allow(clippy::too_many_arguments)]
fn identity_evidence(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    uid: u32,
    root_id: &str,
    root: &area::RootObservation,
    stored: Option<&crate::identity::RootIdentity>,
) -> IdentityEvidence {
    let marker_digest = root.marker_digest.clone();
    let inode = stored.map_or(root.inode, |record| Some(record.inode));
    let device_at_record = stored.map(|record| record.device_at_record);
    match identity::verify(state_paths, dirs, facts, mounts, uid, root_id) {
        IdentityVerdict::Verified {
            remount,
            mount_moved,
        } => IdentityEvidence {
            verdict: "verified".to_owned(),
            reason: None,
            inode,
            device_at_record: Some(remount.device_at_record),
            device_live: Some(remount.device_live),
            remount: Some(RemountEvidenceRow {
                device_changed: remount.changed,
            }),
            mount_moved: Some(mount_moved),
            marker_digest,
        },
        IdentityVerdict::Unknown { reason } => IdentityEvidence {
            verdict: "unknown".to_owned(),
            reason: Some(identity_reason(reason).to_owned()),
            inode,
            device_at_record,
            device_live: root.device,
            remount: None,
            mount_moved: None,
            marker_digest,
        },
        IdentityVerdict::Gone => IdentityEvidence {
            verdict: "gone".to_owned(),
            reason: None,
            inode,
            device_at_record,
            device_live: root.device,
            remount: None,
            mount_moved: None,
            marker_digest,
        },
    }
}

/// A reason code's snake-case spelling, which is the design's reason table.
fn identity_reason(reason: IdentityReason) -> &'static str {
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

/// One root's row when its own lock could not be observed.
///
/// Nothing about the root is read: a shared lock that cannot be taken means a live
/// run may be mutating it, and an observation of a root mid-delete is exactly the
/// stale fact the gate refuses to act on. The row keeps that root only — it is
/// never a candidate — and names the fact rather than staying silent.
fn locked_row(
    root_id: &str,
    mount_relative_path: String,
    root: &area::RootObservation,
    detail: String,
) -> PrunePlanRow {
    PrunePlanRow {
        root_id: root_id.to_owned(),
        mount_relative_path,
        identity: IdentityEvidence {
            verdict: "unknown".to_owned(),
            reason: None,
            inode: root.inode,
            device_at_record: None,
            device_live: root.device,
            remount: None,
            mount_moved: None,
            marker_digest: None,
        },
        custody: "unknown".to_owned(),
        custody_reason: Some(detail.clone()),
        idle: IdleEvidence {
            last_activity_ms: None,
            age_seconds: None,
            declared_age_seconds: None,
            reason: Some(detail.clone()),
        },
        references: Vec::new(),
        references_complete: false,
        absence: None,
        bytes: None,
        eligible: false,
        blocked_by: Some("lock".to_owned()),
        reason: Some(detail),
    }
}

/// The deletion unit one listed root is, when its identity facts were observed.
///
/// The same shape the space prune builds, so both paths ask custody exactly the
/// same question. The unit's `plan_digest` is the zero digest here: this plan is
/// evidence, not the authorization an effect presents (F4 writes the real one).
fn unit_for(
    record: &VolumeRecord,
    resolved: &area::ResolvedArea,
    root: &area::RootObservation,
) -> Option<DeletionUnit> {
    if root.state != MarkerStateName::Owned {
        return None;
    }
    let device = root.device?;
    let inode = root.inode?;
    let marker_digest = root.marker_digest.clone()?;
    Some(DeletionUnit {
        volume_uuid: record.volume_uuid.clone(),
        root_dir: record.root_dir.clone(),
        folder: resolved.folder.clone(),
        parent: String::new(),
        name: root.name.clone(),
        kind: UnitKind::Directory,
        device: DeviceId::new(device),
        inode,
        marker: Some(UnitMarker::new(MarkerFamily::Barge, marker_digest)),
        grant: UnitGrant::native_root(root.name.clone()),
        plan_digest: "0".repeat(64),
    })
}

/// Prove every distinct consumer path the references require.
///
/// More than one distinct path, or one path two records disagree about, is not
/// guessed at: the row is kept. A proof that is not `absent` keeps the row too,
/// and the proof itself travels as the evidence either way.
fn prove_references(
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    requests: &[reference::AbsenceRequest],
) -> (Option<AbsenceEvidence>, Option<String>) {
    if requests.is_empty() {
        return (None, None);
    }
    let mut distinct: Vec<&reference::AbsenceRequest> = Vec::new();
    for candidate in requests {
        match distinct.iter().find(|seen| seen.path == candidate.path) {
            Some(seen) if seen.volume != candidate.volume => {
                return (
                    None,
                    Some(format!(
                        "two records disagree about the volume of `{}`",
                        candidate.path.display()
                    )),
                )
            }
            Some(_) => {}
            None => distinct.push(candidate),
        }
    }
    if distinct.len() > 1 {
        return (
            None,
            Some(
                "the root's references require absence of more than one consumer path, which this plan does not decide"
                    .to_owned(),
            ),
        );
    }
    let wanted = distinct[0];
    let evidence = match absence_for(dirs, facts, &wanted.path, wanted.volume.as_ref()) {
        Ok(evidence) => evidence,
        Err(error) => unknown_absence(&wanted.path, error.message()),
    };
    let blocked = (evidence.verdict != "absent").then(|| {
        format!(
            "the consumer path `{}` is {}: {}",
            evidence.path.display(),
            evidence.verdict,
            evidence.reason.as_deref().unwrap_or("no reason reported")
        )
    });
    (Some(evidence), blocked)
}

/// Prove one consumer path absent, with the ancestor its record names.
fn absence_for(
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    path: &Path,
    volume: Option<&VolumeUuid>,
) -> Result<AbsenceEvidence> {
    let prefix = facts.volume_prefix(path);
    let expected = match prefix.as_ref() {
        // A path under the platform's mount parent is proved against the mount
        // of the volume the record names; without that UUID it cannot be proved
        // at all, and a missing UUID is never read as the boot volume.
        Some(prefix) => {
            let Some(uuid) = volume else {
                return Ok(unknown_absence(
                    path,
                    "the record names no volume for a path under the mount parent",
                ));
            };
            AncestorExpected::Registered {
                uuid,
                recorded_mount: prefix.as_path(),
            }
        }
        None => AncestorExpected::BootData,
    };
    let proof = crate::os::prove_absence(dirs, facts, path, expected)?;
    let value = serde_json::to_value(&proof).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("the absence proof cannot be encoded: {error}"),
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("the absence proof is not the document this plan reads: {error}"),
        )
    })
}

/// One path below a mount root, as the identity record spells it.
fn relative_to_mount(path: &Path, mount: &Path) -> String {
    path.strip_prefix(mount)
        .ok()
        .and_then(Path::to_str)
        .map_or_else(|| path.to_string_lossy().into_owned(), ToOwned::to_owned)
}

/// A proof that could not be established at all, as the document spells it.
fn unknown_absence(path: &Path, reason: impl Into<String>) -> AbsenceEvidence {
    AbsenceEvidence {
        schema: "barge.absence/1".to_owned(),
        path: path.to_path_buf(),
        verdict: ABSENCE_VERDICT_UNKNOWN.to_owned(),
        anchor: AbsenceAnchor {
            kind: "root".to_owned(),
            path: PathBuf::from("/"),
        },
        ancestor: None,
        leaf: None,
        components_walked: 0,
        proved_ms: crate::state::now_ms(),
        reason: Some(reason.into()),
    }
}

/// The plan's canonical bytes: one compact JSON document, exactly the bytes the
/// plan digest covers and a real run must present.
pub fn canonical_plan_bytes(plan: &PrunePlan) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(plan).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the {PRUNE_PLAN_SCHEMA} plan: {error}"),
        )
    })?;
    if bytes.len() > DOCUMENT_SIZE_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the {PRUNE_PLAN_SCHEMA} plan is {} bytes, over the {DOCUMENT_SIZE_CAP_BYTES}-byte document cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", DOCUMENT_SIZE_CAP_BYTES.into()));
    }
    Ok(bytes)
}

/// Read one `barge.prune.plan/1` document from its own bytes.
///
/// A reader refuses an unknown major, a document over the size cap, a body that
/// does not carry exactly this major's field set, and a list whose counts
/// disagree with its rows. The plan is the evidence a later effect presents, so
/// a document whose meaning a reader would have to guess at is not one.
pub fn read_prune_plan(bytes: &[u8]) -> Result<PrunePlan> {
    let document = crate::contract::parse_document(bytes)?;
    let found = document
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                "the plan has no schema string, so it is not a release plan",
            )
        })?;
    if found != PRUNE_PLAN_SCHEMA {
        let prefix = "barge.prune.plan/";
        let message = if found.starts_with(prefix) {
            format!("`{found}` is an unknown major; this reader knows {PRUNE_PLAN_SCHEMA}")
        } else {
            format!("`{found}` is not a {prefix} document")
        };
        return Err(Error::new(ErrorKind::UnknownState, message)
            .with_detail("schema", found.into())
            .with_detail("supported", PRUNE_PLAN_SCHEMA.into()));
    }
    let plan: PrunePlan = serde_json::from_value(document).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "the plan does not provide the field set {PRUNE_PLAN_SCHEMA} requires: {error}"
            ),
        )
    })?;
    plan.rows.validate()?;
    if plan.eligible_count != plan.rows.rows.iter().filter(|row| row.eligible).count() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the plan's eligible count disagrees with its rows",
        )
        .with_detail("eligible_count", plan.eligible_count.into()));
    }
    Ok(plan)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::area::{AreaDeclaration, HoldRequest, HoldScope, OwnedRoot};
    use crate::testkit::{FakeAccount, FakeObserver, FakeVolume, VolumeFixture, FIXTURE_UUID};

    /// The declared age every fixture area uses.
    const AGE_SECONDS: u64 = 86_400;

    /// One fixture: a volume, its record, an account home and an area.
    struct Fixture {
        volume: VolumeFixture,
        account: FakeAccount,
        declaration: AreaDeclaration,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let volume = VolumeFixture::new(label);
            volume.register();
            let account = FakeAccount {
                home: volume.dir.path().to_path_buf(),
            };
            let mut declaration = volume.declaration("pool");
            declaration.prune = PrunePolicy::OldestIdleAfter {
                age_seconds: AGE_SECONDS,
            };
            area::declare(&volume.state, &declaration, 1_000).expect("the area is declared");
            Self {
                volume,
                account,
                declaration,
            }
        }

        fn root(&self) -> OwnedRoot {
            self.volume
                .create_root(
                    &self.volume.record,
                    &self.declaration,
                    area::RootKind::AreaRoot,
                )
                .0
        }

        /// A clock past every freshly created root's age.
        fn overdue(&self) -> u64 {
            crate::state::now_ms().saturating_add(AGE_SECONDS.saturating_mul(1000))
        }

        fn request(&self, now_ms: u64) -> ReleaseRequest<'static> {
            ReleaseRequest {
                namespace: "example",
                area: "pool",
                volume_uuid: Box::leak(Box::new(self.volume.uuid.clone())),
                now_ms,
                bounds: ReleaseBounds::design(),
            }
        }

        fn plan_with(
            &self,
            observer: &FakeObserver,
            facts: &FakeVolume,
            now_ms: u64,
        ) -> Result<PruneReport> {
            self.plan_with_bounds(observer, facts, now_ms, ReleaseBounds::design())
        }

        fn plan_with_bounds(
            &self,
            observer: &FakeObserver,
            facts: &FakeVolume,
            now_ms: u64,
            bounds: ReleaseBounds,
        ) -> Result<PruneReport> {
            let mounts = self.volume.mounts();
            let request = ReleaseRequest {
                bounds,
                ..self.request(now_ms)
            };
            plan(
                &self.volume.state,
                &self.volume.platform(),
                facts,
                &mounts,
                observer,
                &self.account,
                &request,
            )
        }

        /// One plan at the default clock, past the declared age.
        fn plan(&self) -> PruneReport {
            let observer = self.volume.observer();
            let facts = self.volume.facts();
            self.plan_with(&observer, &facts, self.overdue())
                .expect("the plan is built")
        }

        fn plan_at(&self, now_ms: u64) -> PruneReport {
            let observer = self.volume.observer();
            let facts = self.volume.facts();
            self.plan_with(&observer, &facts, now_ms)
                .expect("the plan is built")
        }
    }

    fn row<'a>(report: &'a PruneReport, root: &OwnedRoot) -> &'a PrunePlanRow {
        report
            .plan
            .rows
            .rows
            .iter()
            .find(|row| row.root_id == root.root_id)
            .expect("the plan names the root")
    }

    fn identity_path(fixture: &Fixture, root: &OwnedRoot) -> PathBuf {
        fixture
            .volume
            .state
            .root()
            .join("roots")
            .join(&root.root_id)
            .join("identity.json")
    }

    fn hold(fixture: &Fixture, observer: &FakeObserver, root: &OwnedRoot) {
        area::hold(
            &fixture.volume.state,
            observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "fixture-holder".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Read,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is recorded");
    }

    #[test]
    fn all_six_conditions_release_a_root_and_each_one_alone_keeps_it() {
        // The positive case first: an owned, recorded, idle, unreferenced root on
        // an expendable volume whose area declares an age releases.
        let fixture = Fixture::new("release-positive");
        let root = fixture.root();
        let report = fixture.plan();
        let released = row(&report, &root);
        assert!(released.eligible, "{released:?}");
        assert_eq!(released.identity.verdict, "verified");
        assert_eq!(released.custody, "safe");
        assert!(released.references_complete);
        assert!(released.absence.is_none(), "no reference needs a proof");
        assert!(!report.needs_human(), "an ordinary release needs no human");
        assert_eq!(report.plan.eligible_count, 1);
        assert_eq!(report.plan.examined_count, 1);

        // Condition 2: no durable identity record keeps the root.
        let missing = Fixture::new("release-identity");
        let missing_root = missing.root();
        std::fs::remove_file(identity_path(&missing, &missing_root))
            .expect("the record is removed");
        let report = missing.plan();
        let kept = row(&report, &missing_root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("identity"));
        assert_eq!(
            kept.identity.reason.as_deref(),
            Some("identity_record_missing")
        );
        assert!(
            std::fs::metadata(&missing_root.path).is_ok(),
            "the root is untouched"
        );

        // Condition 3: a live hold keeps the root, and its reference row says so.
        let held = Fixture::new("release-custody");
        let held_root = held.root();
        let mut observer = held.volume.observer();
        observer
            .processes
            .insert(std::process::id(), Some(7_000_000));
        hold(&held, &observer, &held_root);
        let facts = held.volume.facts();
        let report = held
            .plan_with(&observer, &facts, held.overdue())
            .expect("the plan is built");
        let kept = row(&report, &held_root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("custody"));
        assert_ne!(kept.custody, "safe");
        assert!(kept
            .references
            .iter()
            .any(|reference| reference.source == reference::BARGE_HOLD && reference.current));

        // Condition 4: a root younger than the declared age is kept.
        let young = Fixture::new("release-young");
        let young_root = young.root();
        let report = young.plan_at(crate::state::now_ms());
        let kept = row(&report, &young_root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("policy"));

        // Condition 4 again: an area that declares no age releases nothing.
        let undeclared = Fixture::new("release-policy-none");
        let undeclared_root = undeclared.root();
        let mut declaration = undeclared.declaration.clone();
        declaration.prune = PrunePolicy::None;
        area::declare(&undeclared.volume.state, &declaration, 2_000).expect("the policy changes");
        let report = undeclared.plan();
        let kept = row(&report, &undeclared_root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("policy"));
        assert!(kept
            .references
            .iter()
            .any(|reference| { reference.source == reference::BARGE_AREA && reference.current }));

        // Condition 5: a placement that names the root is current.
        let placed = Fixture::new("release-references");
        let placed_root = placed.root();
        publish_placement(&placed, &placed_root);
        let report = placed.plan();
        let kept = row(&report, &placed_root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("references"));
        assert!(kept.references.iter().any(|reference| {
            reference.source == reference::BARGE_PLACEMENT && reference.current
        }));
    }

    #[test]
    fn an_absent_volume_plans_nothing_and_a_permanent_one_is_loud() {
        for (availability, loud, exit) in [
            (Availability::Ephemeral, false, false),
            (Availability::Permanent, true, true),
        ] {
            let fixture = Fixture::new("release-absent");
            let mut record = fixture.volume.record.clone();
            record.availability = availability;
            fixture.volume.register_record(&record);
            let facts = fixture.volume.facts();
            facts.present.set(false);
            let observer = fixture.volume.observer();
            let report = fixture
                .plan_with(&observer, &facts, fixture.overdue())
                .expect("an absent volume is a report");
            assert_eq!(report.volume.state, VolumeState::Absent);
            assert_eq!(report.volume.loud, loud);
            assert_eq!(report.needs_human(), exit);
            assert_eq!(report.plan.rows.count, 0, "nothing is observed or released");
            assert!(
                !fixture.volume.root.exists()
                    || std::fs::read_dir(&fixture.volume.root)
                        .expect("the mount root lists")
                        .next()
                        .is_none(),
                "no mount point was recreated"
            );
        }
    }

    #[test]
    fn an_undeclared_area_and_an_unregistered_volume_are_refused_before_any_effect() {
        let fixture = Fixture::new("release-refusals");
        let mounts = fixture.volume.mounts();
        let observer = fixture.volume.observer();
        let facts = fixture.volume.facts();
        let undeclared = ReleaseRequest {
            namespace: "example",
            area: "elsewhere",
            volume_uuid: &fixture.volume.uuid,
            now_ms: fixture.overdue(),
            bounds: ReleaseBounds::design(),
        };
        let error = plan(
            &fixture.volume.state,
            &fixture.volume.platform(),
            &facts,
            &mounts,
            &observer,
            &fixture.account,
            &undeclared,
        )
        .expect_err("an undeclared area is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);

        let other = VolumeUuid::parse(crate::testkit::OTHER_UUID).expect("a UUID");
        let unregistered = ReleaseRequest {
            volume_uuid: &other,
            ..undeclared
        };
        let error = plan(
            &fixture.volume.state,
            &fixture.volume.platform(),
            &facts,
            &mounts,
            &observer,
            &fixture.account,
            &unregistered,
        )
        .expect_err("an unregistered volume is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_inode_change_keeps_the_root() {
        // The event that must keep a root: the directory verified is not the
        // directory that was recorded. The device-only half of the carried
        // finding is `identity`'s own named pair, which proves a changed device
        // verifies and the old recorded-device comparison would have refused.
        let other = Fixture::new("release-inode");
        let other_root = other.root();
        let marker = std::fs::read(other_root.path.join(area::ROOT_MARKER_FILE))
            .expect("the marker is read");
        let moved = other_root.path.with_extension("moved");
        std::fs::rename(&other_root.path, &moved).expect("the root is moved aside");
        std::fs::create_dir(&other_root.path).expect("a new directory takes the name");
        std::fs::set_permissions(&other_root.path, std::fs::Permissions::from_mode(0o700))
            .expect("the directory is private");
        let copied = other_root.path.join(area::ROOT_MARKER_FILE);
        std::fs::write(&copied, &marker).expect("the marker is copied");
        std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o600))
            .expect("the copied marker is private, as every record is");
        let observer = other.volume.observer();
        let facts = other.volume.facts();
        let report = other
            .plan_with(&observer, &facts, other.overdue())
            .expect("the plan is built");
        let kept = row(&report, &other_root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("identity"));
        assert_eq!(kept.identity.reason.as_deref(), Some("inode_changed"));
    }

    #[test]
    fn last_activity_is_the_newest_modification_time_and_a_future_one_is_unknown() {
        let fixture = Fixture::new("release-activity");
        let root = fixture.root();
        // A payload written after the root was created is the newest activity.
        VolumeFixture::payload(&root.path, "target/debug/artifact", b"bytes");
        let report = fixture.plan_at(crate::state::now_ms());
        let kept = row(&report, &root);
        assert_eq!(kept.blocked_by.as_deref(), Some("policy"));
        let last = kept
            .idle
            .last_activity_ms
            .expect("the scan found an activity");
        assert!(
            last >= std::fs::metadata(root.path.join("target/debug/artifact"))
                .expect("the artifact exists")
                .modified()
                .expect("the artifact has a modification time")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is after the epoch")
                .as_millis() as u64,
            "the scan reports the payload's own modification time"
        );
        assert!(kept
            .bytes
            .is_some_and(|bytes| bytes.complete && bytes.logical > 0));

        // A modification time ahead of this run's clock is unknown, never idle.
        let future = SystemTime::now() + Duration::from_secs(3_600);
        File::options()
            .write(true)
            .open(root.path.join("target/debug/artifact"))
            .expect("the artifact opens")
            .set_modified(future)
            .expect("the modification time is set");
        let report = fixture.plan();
        let kept = row(&report, &root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("policy"));
        assert!(kept.idle.last_activity_ms.is_none());
        assert!(kept
            .idle
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("ahead of this run's clock")));
    }

    #[test]
    fn a_root_deeper_than_the_scan_bound_is_kept() {
        let fixture = Fixture::new("release-depth");
        let root = fixture.root();
        let mut path = root.path.clone();
        for level in 0..=area::ACTIVITY_DEPTH {
            path = path.join(format!("level-{level}"));
        }
        std::fs::create_dir_all(&path).expect("the deep tree is created");
        let report = fixture.plan();
        let kept = row(&report, &root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("policy"));
        assert!(kept.idle.last_activity_ms.is_none());
        assert!(kept.bytes.is_some_and(|bytes| !bytes.complete));
    }

    #[test]
    fn the_plan_round_trips_and_a_changed_digest_is_a_different_plan() {
        let fixture = Fixture::new("release-round-trip");
        let root = fixture.root();
        let report = fixture.plan();
        let bytes = canonical_plan_bytes(&report.plan).expect("the plan encodes");
        assert_eq!(sha256_hex(&bytes), report.plan_digest);
        let read = read_prune_plan(&bytes).expect("the plan reads");
        assert_eq!(read, report.plan);
        assert_eq!(read.rows.count, 1);
        assert_eq!(read.eligible_count, 1);
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("the plan is JSON");
        assert_eq!(value["schema"], PRUNE_PLAN_SCHEMA);
        assert_eq!(value["area"]["namespace"], "example");
        assert_eq!(value["volume_uuid"], FIXTURE_UUID);
        assert_eq!(value["rows"]["rows"][0]["root_id"], root.root_id);
        assert_eq!(value["rows"]["rows"][0]["eligible"], true);

        // A digest over different bytes does not describe this plan.
        let mut tampered = report.plan.clone();
        tampered.rows.rows[0].reason = Some("rewritten".to_owned());
        assert_ne!(
            sha256_hex(&canonical_plan_bytes(&tampered).expect("the plan encodes")),
            report.plan_digest
        );
        // An extra field is refused: the plan is an authorization, not a hint.
        let mut document: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the plan is JSON");
        document["unexpected"] = true.into();
        assert!(read_prune_plan(&serde_json::to_vec(&document).expect("it encodes")).is_err());
    }

    #[test]
    fn a_kept_volume_releases_nothing_and_says_why() {
        let fixture = Fixture::new("release-kept-volume");
        let root = fixture.root();
        let mut record = fixture.volume.record.clone();
        record.durability = Durability::Kept;
        fixture.volume.register_record(&record);
        let report = fixture.plan();
        assert_eq!(report.plan.rows.count, 0);
        assert!(report
            .volume
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("kept")));
        assert!(std::fs::metadata(&root.path).is_ok());
        assert!(!report.needs_human(), "a kept volume is a quiet refusal");
    }

    #[test]
    fn a_symlink_entry_is_never_released() {
        let fixture = Fixture::new("release-symlink");
        let root = fixture.root();
        let link = root
            .path
            .parent()
            .expect("the area folder")
            .join("a-symlink");
        std::os::unix::fs::symlink(&root.path, &link).expect("the link is created");
        let report = fixture.plan();
        let kept = row(&report, &root);
        assert!(
            kept.eligible,
            "the real root is not penalised by its neighbour"
        );
        let link_row = report
            .plan
            .rows
            .rows
            .iter()
            .find(|row| row.root_id == "a-symlink")
            .expect("the symlink is a row of its own");
        assert!(!link_row.eligible);
        assert_eq!(link_row.blocked_by.as_deref(), Some("identity"));
        std::fs::remove_file(&link).expect("the link is removed");
    }

    /// A lock budget that does not wait: the first refusal is the answer.
    fn no_wait() -> ReleaseBounds {
        ReleaseBounds {
            lock_wait: Duration::ZERO,
            ..ReleaseBounds::design()
        }
    }

    /// Create one lock file by taking it exclusively and releasing it.
    fn touch_lock(path: &PathBuf, rank: state::LockRank) {
        drop(
            state::StateLock::exclusive(path, rank, Deadline::after(Duration::from_secs(5)))
                .expect("the fixture lock file is created"),
        );
    }

    #[test]
    fn a_held_root_lock_excludes_only_that_root() {
        let fixture = Fixture::new("release-held-root");
        let first = fixture.root();
        let second = fixture.root();
        let path = fixture
            .volume
            .state
            .root_lock_path(&second.root_id)
            .expect("the root lock path");
        let held = state::StateLock::exclusive(
            &path,
            state::LockRank::Root,
            Deadline::after(Duration::from_secs(5)),
        )
        .expect("the fixture holds the root's lock");

        let observer = fixture.volume.observer();
        let facts = fixture.volume.facts();
        let report = fixture
            .plan_with_bounds(&observer, &facts, fixture.overdue(), no_wait())
            .expect("a held root lock is a row, not a refusal");
        let blocked = row(&report, &second);
        assert!(!blocked.eligible);
        assert_eq!(blocked.blocked_by.as_deref(), Some("lock"));
        assert!(
            blocked
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("nothing about this root was read")),
            "{blocked:?}"
        );
        assert!(
            !blocked.references_complete,
            "nothing about the root was read"
        );
        assert!(blocked.bytes.is_none(), "nothing about the root was read");
        // The other root of the same area still plans and is a candidate.
        let released = row(&report, &first);
        assert!(released.eligible, "{released:?}");
        assert_eq!(report.plan.eligible_count, 1);
        assert_eq!(report.plan.examined_count, 2);
        assert!(!report.needs_human(), "one busy root needs no human");
        assert!(
            std::fs::metadata(&second.path).is_ok(),
            "nothing was changed"
        );
        drop(held);
    }

    #[test]
    fn a_held_volume_lock_refuses_the_preview_and_names_the_lock_file() {
        let fixture = Fixture::new("release-held-volume");
        let root = fixture.root();
        let path = fixture.volume.state.volume_lock_path(&fixture.volume.uuid);
        let held = state::StateLock::exclusive(
            &path,
            state::LockRank::Volume,
            Deadline::after(Duration::from_secs(5)),
        )
        .expect("the fixture holds the volume's lock");

        let mounts = fixture.volume.mounts();
        let observer = fixture.volume.observer();
        let facts = fixture.volume.facts();
        let request = ReleaseRequest {
            bounds: no_wait(),
            ..fixture.request(fixture.overdue())
        };
        let error = plan(
            &fixture.volume.state,
            &fixture.volume.platform(),
            &facts,
            &mounts,
            &observer,
            &fixture.account,
            &request,
        )
        .expect_err("a held volume lock refuses this volume's preview");
        assert_eq!(error.kind(), ErrorKind::Locked);
        assert_eq!(
            error.detail().get("lock").and_then(|value| value.as_str()),
            Some(path.display().to_string().as_str()),
            "the refusal names the lock file"
        );
        assert!(
            error
                .message()
                .contains(&fixture.volume.uuid.as_str().to_owned()),
            "the refusal names the volume"
        );
        assert!(std::fs::metadata(&root.path).is_ok(), "nothing was changed");
        drop(held);
    }

    #[test]
    fn the_plan_observes_the_volume_before_a_root_and_never_the_reverse() {
        // The order the plan relies on, proved through the rank check rather than
        // asserted about the code: volume then root is accepted, root then volume
        // is a usage refusal before any file is touched.
        let fixture = Fixture::new("release-lock-order");
        let root = fixture.root();
        let volume_path = fixture.volume.state.volume_lock_path(&fixture.volume.uuid);
        let root_path = fixture
            .volume
            .state
            .root_lock_path(&root.root_id)
            .expect("the root lock path");
        touch_lock(&volume_path, state::LockRank::Volume);
        touch_lock(&root_path, state::LockRank::Root);

        let mut locks = state::LockSet::with_deadline(
            fixture.volume.state.clone(),
            Deadline::after(Duration::from_secs(5)),
        );
        let volume = locks
            .observe_volume(&fixture.volume.uuid)
            .expect("the volume is observed");
        assert!(volume.is_some(), "an existing lock file is observed");
        let owned = locks
            .observe_root(&root.root_id)
            .expect("the root is observed after the volume");
        assert!(owned.is_some());
        drop((volume, owned));

        let mut reverse = state::LockSet::with_deadline(
            fixture.volume.state.clone(),
            Deadline::after(Duration::from_secs(5)),
        );
        let owned = reverse
            .observe_root(&root.root_id)
            .expect("the root is observed first here");
        let error = reverse
            .observe_volume(&fixture.volume.uuid)
            .expect_err("the volume after a root is out of order");
        assert_eq!(error.kind(), ErrorKind::Usage);
        drop(owned);
    }

    /// Publish one valid placement record whose recorded path is this root.
    fn publish_placement(fixture: &Fixture, root: &OwnedRoot) {
        let observed = crate::cache::PreAdoption {
            device: root.identity.device.get(),
            inode: root.identity.inode,
            entries: 1,
            newest_modified_ms: None,
            allocated_bytes: 0,
        };
        let form = crate::cache::PlacementForm::HeldRoot(crate::cache::HeldRootForm {
            path: root.path.clone(),
            publication: None,
            held_root: crate::cache::HeldRoot {
                path: root.path.clone(),
                root_id: root.root_id.clone(),
                nonce: root.nonce.clone(),
                device: root.identity.device.get(),
                inode: root.identity.inode,
                marker_digest: root.marker_digest.clone(),
                observed,
            },
        });
        let record = crate::cache::PlacementRecord {
            schema: crate::contract::PLACEMENT_SCHEMA.to_owned(),
            placement_id: "0123456789abcdef0123456789abcdef".to_owned(),
            kind: crate::cache::CacheKind::CargoRegistry,
            original_path: fixture.account.home.join(".cargo/registry"),
            volume_uuid: fixture.volume.uuid.clone(),
            root_dir: String::new(),
            folder: "caches".to_owned(),
            recorded_path: root.path.clone(),
            adopted_ms: 1_000,
            generation: 1,
            form,
        };
        record.validate().expect("the fixture placement is valid");
        let path = crate::cache::placement_path(&fixture.volume.state, &record.placement_id)
            .expect("the placement path");
        let bytes = serde_json::to_vec_pretty(&record).expect("the record encodes");
        state::publish_atomic(&path, &bytes).expect("the record is published");
    }

    /// The control directory a fixture's Oxide records live in.
    fn oxide_control(fixture: &Fixture) -> PathBuf {
        fixture
            .account
            .home
            .join("Library/Caches/oxide/build-pool")
            .join(crate::inspect::OXIDE_CONTROL_DIR)
    }

    fn oxide_registry(fixture: &Fixture, row: &str, complete: bool) {
        let outputs = oxide_control(fixture).join(reference::OXIDE_OUTPUTS_DIR);
        std::fs::create_dir_all(&outputs).expect("the outputs folder is created");
        let document = format!(
            r#"{{"version":"{}","schema":2,"revision":1,"complete":{complete},"created_by":"oxide","rows":[{row}]}}"#,
            reference::OXIDE_REGISTRY_V2
        );
        std::fs::write(outputs.join(reference::OXIDE_REGISTRY_FILE), document)
            .expect("the registry is written");
    }

    #[test]
    fn an_oxide_reference_that_is_not_settled_keeps_the_root() {
        for flags in [
            r#""selected":true,"pending":false"#,
            r#""selected":false,"pending":true"#,
            r#""selected":false,"pending":false,"pruning":"intent-1""#,
            r#""selected":false,"pending":false,"reservation":"reservation-1""#,
            r#""selected":false,"pending":false,"cancelling":"cancel-1""#,
        ] {
            let fixture = Fixture::new("release-oxide-live");
            let root = fixture.root();
            let registry_row = format!(
                r#"{{"checkout_id":"checkout-1","checkout":"{}","receipt":"{}","member":"target","root":"{}","root_kind":"external","recipe":"target-debug-v1","generation":1,{flags}}}"#,
                fixture.account.home.join("checkout").display(),
                fixture.account.home.join("receipt.json").display(),
                root.path.display(),
            );
            oxide_registry(&fixture, &registry_row, true);
            let report = fixture.plan();
            let kept = row(&report, &root);
            assert!(!kept.eligible, "{flags}: {kept:?}");
            assert_eq!(kept.blocked_by.as_deref(), Some("references"));
            assert!(kept.references.iter().any(|reference| {
                reference.source == reference::OXIDE_REGISTRY && reference.current
            }));
        }
    }

    #[test]
    fn a_settled_oxide_row_releases_only_with_an_absent_consumer_path() {
        // Present: the checkout still exists, so the reference's own rule is not
        // satisfied and the root is kept with the proof as its evidence.
        let fixture = Fixture::new("release-oxide-present");
        let root = fixture.root();
        let checkout = fixture.account.home.join("checkout");
        std::fs::create_dir_all(&checkout).expect("the checkout exists");
        let registry_row = format!(
            r#"{{"checkout_id":"checkout-1","checkout":"{}","receipt":"{}","member":"target","root":"{}","root_kind":"external","recipe":"target-debug-v1","generation":1,"selected":false,"pending":false}}"#,
            checkout.display(),
            fixture.account.home.join("receipt.json").display(),
            root.path.display(),
        );
        oxide_registry(&fixture, &registry_row, true);
        let report = fixture.plan();
        let kept = row(&report, &root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("absence"));
        let proof = kept
            .absence
            .as_ref()
            .expect("the proof travels as evidence");
        assert_eq!(proof.verdict, "present");
        assert_eq!(proof.path, checkout);

        // Absent: the checkout is gone, so the whole gate releases.
        std::fs::remove_dir(&checkout).expect("the checkout is removed");
        let report = fixture.plan();
        let released = row(&report, &root);
        assert!(released.eligible, "{released:?}");
        let proof = released
            .absence
            .as_ref()
            .expect("the proof travels as evidence");
        assert_eq!(proof.verdict, "absent");
        assert_eq!(
            proof.leaf.as_ref().map(|leaf| leaf.result.as_str()),
            Some("enoent")
        );
    }

    #[test]
    fn an_incomplete_or_divergent_reference_source_keeps_the_whole_batch() {
        // A registry that reports itself incomplete: every row it might name is
        // kept, and the command says it needs a human.
        let fixture = Fixture::new("release-oxide-incomplete");
        let root = fixture.root();
        let registry_row = format!(
            r#"{{"checkout_id":"checkout-1","checkout":"{}","receipt":"{}","member":"target","root":"{}","root_kind":"external","recipe":"target-debug-v1","generation":1,"selected":false,"pending":false}}"#,
            fixture.account.home.join("checkout").display(),
            fixture.account.home.join("receipt.json").display(),
            root.path.display(),
        );
        oxide_registry(&fixture, &registry_row, false);
        let report = fixture.plan();
        let kept = row(&report, &root);
        assert!(!kept.eligible);
        assert_eq!(kept.blocked_by.as_deref(), Some("references"));
        assert!(!kept.references_complete);
        assert!(report.needs_human(), "an incomplete reference set exits 3");

        // An unknown receipt version keeps the root too: a record this build
        // cannot read is never read as absent.
        let other = Fixture::new("release-oxide-receipt");
        let other_root = other.root();
        let outputs = oxide_control(&other).join(reference::OXIDE_OUTPUTS_DIR);
        std::fs::create_dir_all(&outputs).expect("the outputs folder is created");
        std::fs::write(
            outputs.join("checkout-1.json"),
            br#"{"version":"oxide.outputs.transition/9","checkout":"/fixture/checkout","checkout_id":"checkout-1","generation":1,"state":"committed","links":[],"roots":[],"updated_ms":1}"#,
        )
        .expect("the receipt is written");
        let report = other.plan();
        let kept = row(&report, &other_root);
        assert!(!kept.eligible);
        assert!(!kept.references_complete);
    }
}
