//! Area prune policy evaluation and `maintain`, preview and real (stage 1
//! design, "Scheduling without a daemon").
//!
//! Slice S7 owns the one command a host job calls. The rules it carries:
//!
//! - **One fixed order.** `maintain` sweeps every registered, present volume's
//!   scratch folder (S4) and then each declared area's prune policy, in registry
//!   order and then declaration order, and does nothing else. It never runs
//!   Oxide's outputs prune, reads an Oxide receipt or deletes an Oxide root.
//! - **Preview then real.** One bounded canonical `barge.maintain.plan/1` and its
//!   SHA-256. A preview changes nothing; a real run requires exactly that digest
//!   (and, when the caller kept it, those exact plan bytes), then re-verifies
//!   every predicate immediately before each effect: the embedded scratch plan
//!   digest, the root's identity and marker digest, the registry record, the
//!   declaration's current prune policy and the root's holds.
//! - **Absence is a report, never an effect.** An absent `ephemeral` volume is
//!   skipped and reported; an absent `permanent` volume is reported loudly while
//!   the other volumes continue. A mount point is never recreated, and an
//!   undeclared or `kept` volume's data is never pruned.
//! - **A bound leaves the rest for later.** The run's wall clock is the design's
//!   600 s with a 30 s stop window; reaching it reports `timed_out` and the work
//!   it did not reach, and it is never reported as success.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::area::{self, AreaDocument, MarkerStateName, PrunePolicy};
use crate::contract::{
    RowSet, AREA_ROWS, DOCUMENT_SIZE_CAP_BYTES, INTENT_ROWS, MAINTAIN_PLAN_SCHEMA, ROOT_ROWS,
    VOLUME_ROWS,
};
use crate::delete::{self, DeleteBounds, PendingRow};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{AbsenceVolume, DeviceId, DirOps, MountRoot, ProcessObserver, VolumeFacts};
use crate::registry::{self, Availability, Durability, VolumeRecord};
use crate::scratch::{self, SweepBounds, SweepPlan, SweepPolicy, SweepRequest};
use crate::state::{self, StatePaths};
use crate::unit::{self, DeletionUnit, MarkerFamily, UnitGrant, UnitKind, UnitMarker};

/// How long one `maintain` run may take (design, "Bounded resources").
pub const MAINTAIN_WALL: Duration = Duration::from_secs(600);

/// How much of that wall clock is reserved for stopping: no new effect starts
/// inside it, so the run reports instead of being killed mid-effect.
pub const MAINTAIN_STOP_MARGIN: Duration = Duration::from_secs(30);

/// One run's bounds. The design's are 600 s and 30 s; a test shortens them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintainBounds {
    /// The whole run's wall clock.
    pub wall: Duration,
    /// The window at the end in which no new effect starts.
    pub stop_margin: Duration,
}

impl MaintainBounds {
    /// The design's bounds.
    pub const fn design() -> Self {
        Self {
            wall: MAINTAIN_WALL,
            stop_margin: MAINTAIN_STOP_MARGIN,
        }
    }

    /// The moment no new effect may start.
    fn deadline(self, started: Instant) -> Instant {
        started + self.wall.saturating_sub(self.stop_margin)
    }
}

impl Default for MaintainBounds {
    fn default() -> Self {
        Self::design()
    }
}

/// Whether a volume was attached when the plan was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeState {
    /// Attached and verified.
    Present,
    /// Not attached. Its recorded mount point is never written to or recreated.
    Absent,
}

/// One volume's step in the maintain plan: its scratch sweep and its areas.
///
/// Everything here is stable across a preview and its real run, so the plan's
/// digest is reproducible; the time the plan was built and every effect it
/// caused are reported outside the plan, in `barge.maintain/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePlan {
    /// The registered volume.
    pub volume_uuid: String,
    /// When the volume is attached.
    pub availability: Availability,
    /// What the data on it is.
    pub durability: Durability,
    /// The last observed mount point.
    pub mount_hint: Option<PathBuf>,
    /// The volume's recorded `root_dir`.
    pub root_dir: String,
    /// The registry record's generation when the plan was built. The effect
    /// step compares it under the volume lock, so a record changed in the run's
    /// own window stops the affected units instead of being trusted.
    pub record_generation: u64,
    /// Whether it was attached.
    pub state: VolumeState,
    /// Why the volume has no work, when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the report needs a human: a permanent volume's absence, or a
    /// volume whose durability an operator has not declared.
    pub loud: bool,
    /// The volume's scratch sweep, when the volume is present and prunable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<ScratchStep>,
    /// Each declared area's prune step, in declaration order.
    pub areas: RowSet<AreaPrunePlan>,
}

impl VolumePlan {
    /// Whether any work was planned for this volume.
    pub fn has_work(&self) -> bool {
        self.scratch.is_some() || !self.areas.rows.is_empty()
    }
}

/// The scratch stage of one volume: its plan and the digest that authorizes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchStep {
    /// The SHA-256 of the scratch plan's own canonical bytes.
    pub plan_digest: String,
    /// The stable scratch plan (S4's `barge.scratch.plan/1`).
    pub plan: SweepPlan,
}

/// One declared area's prune step on one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaPrunePlan {
    /// The area's namespace.
    pub namespace: String,
    /// The area's name.
    pub area: String,
    /// The folder this volume resolves the area to.
    pub folder: String,
    /// The declared policy the step applied.
    pub policy: PrunePolicy,
    /// The owned roots whose idleness is past the declared age.
    pub candidates: RowSet<PruneRootRow>,
    /// The roots this step keeps, with the reason.
    pub kept: RowSet<PruneKeptRow>,
    /// How many folder entries the listing examined.
    pub examined: usize,
    /// Whether a listing bound stopped the walk before the folder was exhausted.
    pub truncated: bool,
}

/// One owned root the plan would prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruneRootRow {
    /// The root's id, which is its name inside the area folder.
    pub name: String,
    /// The path the current configuration resolves.
    pub path: PathBuf,
    /// The root directory's device.
    pub device: u64,
    /// The root directory's inode on that device.
    pub inode: u64,
    /// The SHA-256 of the root's exact marker bytes.
    pub marker_digest: String,
    /// What the root is for.
    pub kind: area::RootKind,
    /// When the root was created, as its marker records it.
    pub created_ms: Option<u64>,
    /// The newest activity Barge observed for the root.
    pub last_activity_ms: u64,
    /// Why it is eligible, in one line.
    pub reason: String,
}

/// One root the plan keeps, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruneKeptRow {
    /// The entry's name inside the area folder.
    pub name: String,
    /// The observed path, when one was resolved.
    pub path: Option<PathBuf>,
    /// What it is: `owned`, `absent`, `unknown` or an entry kind.
    pub state: String,
    /// Why it is kept.
    pub reason: String,
}

/// The `barge.maintain.plan/1` body: one bounded canonical plan and nothing
/// volatile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintainPlan {
    /// Always `barge.maintain.plan/1`.
    pub schema: String,
    /// The scratch ages the plan applied.
    pub policy: SweepPolicy,
    /// The registered volumes, in registry order.
    pub volumes: RowSet<VolumePlan>,
}

impl MaintainPlan {
    /// How many units the plan would delete, across every volume.
    pub fn candidate_count(&self) -> usize {
        self.volumes
            .rows
            .iter()
            .map(|volume| {
                volume
                    .scratch
                    .as_ref()
                    .map_or(0, |step| step.plan.candidates.count)
                    + volume
                        .areas
                        .rows
                        .iter()
                        .map(|area| area.candidates.count)
                        .sum::<usize>()
            })
            .sum()
    }
}

/// One unit a real run deleted from a declared area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaDeletedRow {
    /// The root's name.
    pub name: String,
    /// Its path before the deletion.
    pub path: PathBuf,
    /// The intent that recorded it.
    pub intent_id: String,
    /// How many entries the deletion removed.
    pub entries_removed: usize,
    /// How many directories it removed, the quarantine leaf included.
    pub directories_removed: usize,
    /// Whether the marker was unlinked last.
    pub marker_removed: bool,
}

/// One planned root a real run kept, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaSkippedRow {
    /// The root's name.
    pub name: String,
    /// Why it was kept this run.
    pub reason: String,
}

/// What one volume's scratch stage did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchOutcome {
    /// The digest the stage was authorized by.
    pub plan_digest: String,
    /// The units deleted.
    pub deleted: RowSet<scratch::DeletedRow>,
    /// The intents left pending.
    pub pending: RowSet<PendingRow>,
    /// The candidates not deleted.
    pub skipped: RowSet<scratch::SkippedRow>,
    /// What the stage's replay of earlier pending intents did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<delete::ReplayReport>,
}

impl ScratchOutcome {
    /// Whether the stage left no work unsettled.
    fn is_settled(&self) -> bool {
        self.pending.count == 0
            && self
                .replay
                .as_ref()
                .is_none_or(delete::ReplayReport::is_settled)
    }
}

/// What one declared area's prune stage did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaPruneOutcome {
    /// The area's namespace.
    pub namespace: String,
    /// The area's name.
    pub area: String,
    /// The roots deleted.
    pub deleted: RowSet<AreaDeletedRow>,
    /// The planned roots kept this run.
    pub skipped: RowSet<AreaSkippedRow>,
    /// The intents left pending.
    pub pending: RowSet<PendingRow>,
}

impl AreaPruneOutcome {
    /// A stage that did nothing yet, which is what a preview reports.
    fn empty(namespace: &str, area: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            area: area.to_owned(),
            deleted: RowSet::empty(),
            skipped: RowSet::empty(),
            pending: RowSet::empty(),
        }
    }
}

/// What one volume's real run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeOutcome {
    /// The volume's UUID.
    pub volume_uuid: String,
    /// Whether it was attached.
    pub state: VolumeState,
    /// Whether the report needs a human for this volume.
    pub loud: bool,
    /// Why the volume had no work, or why the run stopped before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The scratch stage's outcome, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<ScratchOutcome>,
    /// Each area stage's outcome, in plan order.
    pub areas: Vec<AreaPruneOutcome>,
}

/// What was not reached because the run's wall clock ran out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnreachedRow {
    /// The volume the run stopped on.
    pub volume_uuid: String,
    /// The stage it stopped before: `scratch`, or `area:<namespace>/<area>`.
    pub stage: String,
    /// How many planned candidates of that stage were not attempted.
    pub candidates: usize,
}

/// The `barge.maintain/1` data: the plan, its digest, and what the run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintainReport {
    /// Whether this run only previewed.
    pub preview: bool,
    /// The SHA-256 of the plan's canonical bytes.
    pub plan_digest: String,
    /// When the plan was built. It is outside the plan, so the digest is stable.
    pub planned_ms: u64,
    /// Whether the run's wall clock stopped it before all its work was done.
    pub timed_out: bool,
    /// Why a real run stopped before the rest of its plan, when one did. A
    /// failure after the first effect is never exit `2` ("refused before any
    /// effect"), so it is reported here and the command exits `3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// How many units the plan would delete.
    pub candidates: usize,
    /// The stable plan.
    pub plan: MaintainPlan,
    /// What each volume's real run did. A preview reports none.
    pub volumes: RowSet<VolumeOutcome>,
    /// Every intent the run left pending, across all stages.
    pub pending: RowSet<PendingRow>,
    /// The stages the wall clock stopped before.
    pub unreached: RowSet<UnreachedRow>,
    /// Whether the optional space stage ran.
    #[serde(default = "disabled_space_stage")]
    pub space_stage: String,
    /// Per-volume space previews or decisions, outside the canonical plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_rows: Option<RowSet<serde_json::Value>>,
    /// Number of volumes whose preview crossed its declared trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_triggered: Option<usize>,
}

fn disabled_space_stage() -> String {
    "disabled".to_owned()
}

impl MaintainReport {
    /// The exit code the finished document forces.
    ///
    /// `3` is the design's "a pending intent or unknown state that needs a
    /// human": a pending intent, a replay that did not settle, a permanent
    /// volume that is not attached, a volume whose durability is undeclared, or
    /// a run its wall clock stopped. `0` is everything else, including a partial
    /// completion with nothing pending.
    pub fn needs_human(&self) -> bool {
        self.timed_out
            || self.stopped.is_some()
            || self.pending.count > 0
            || self.plan.volumes.rows.iter().any(|volume| volume.loud)
            || self.volumes.rows.iter().any(|volume| {
                volume.loud
                    || volume
                        .scratch
                        .as_ref()
                        .is_some_and(|stage| !stage.is_settled())
            })
    }
}

/// The intents an earlier run left pending, as a preview reports them.
///
/// A preview changes nothing, so it does not replay them; it names them, which is
/// what makes the document force exit `3` and tells the host-job operator that a
/// real run of this plan has work to settle.
fn preview_pending(state_paths: &StatePaths) -> Result<RowSet<PendingRow>> {
    let intents = state::read_intents(state_paths, INTENT_ROWS)?;
    let rows: Vec<PendingRow> = intents
        .pending()
        .map(|intent| PendingRow {
            intent_id: intent.intent_id.clone(),
            unit_name: intent.unit.name.clone(),
            unit_kind: intent.unit.kind.as_str().to_owned(),
            phase: intent.phase,
            reason: "an earlier run left this intent pending; a real run of this plan replays it"
                .to_owned(),
        })
        .collect();
    let total = rows.len();
    Ok(RowSet::bounded(rows, total, ROOT_ROWS))
}

/// What one caller asks `barge maintain` for.
#[derive(Debug, Clone)]
pub struct MaintainRequest<'a> {
    /// Whether to preview only. A preview changes nothing.
    pub preview: bool,
    /// The SHA-256 of the preview's plan bytes. A real run requires it, together
    /// with `plan_bytes`, and refuses when the two disagree.
    pub plan_digest: Option<&'a str>,
    /// The preview's exact plan bytes. A real run requires them: they name the
    /// units it may act on, and every one of those is re-verified before its
    /// effect.
    pub plan_bytes: Option<&'a [u8]>,
    /// The scratch sweep's bounds.
    pub sweep_bounds: SweepBounds,
    /// The deletion's bounds.
    pub delete_bounds: DeleteBounds,
    /// The run's wall clock.
    pub maintain_bounds: MaintainBounds,
    /// The current time, so a test can age a root or a conclusion.
    pub now_ms: u64,
}

/// Preview or run `maintain` across every registered volume.
///
/// A preview builds the plan and its digest and changes nothing. A real run is
/// authorized by that plan: it requires the preview's exact bytes and their
/// digest, applies exactly the units the plan names, and re-verifies every
/// predicate immediately before each effect. A unit that changed in between — a
/// live hold, a withdrawn delegation or claim, a marker that appeared, a changed
/// identity or revision — is kept and reported as that unit only, and the other
/// planned units still go. Scratch is S4's plan applied through
/// [`scratch::expire_plan`] and an area root is S5's custody and delete path;
/// nothing else is touched.
#[allow(clippy::too_many_arguments)]
pub fn maintain(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &MaintainRequest<'_>,
) -> Result<MaintainReport> {
    let started = Instant::now();
    let records = read_registry(state_paths)?;
    let declarations = area::read_declarations(state_paths)?;

    if request.preview {
        let plan = build_plan(
            state_paths,
            dirs,
            facts,
            mounts,
            observer,
            &records,
            &declarations,
            request,
        )?;
        let candidates = plan.candidate_count();
        return Ok(MaintainReport {
            preview: true,
            plan_digest: sha256_hex(&canonical_plan_bytes(&plan)?),
            planned_ms: request.now_ms,
            timed_out: false,
            stopped: None,
            candidates,
            plan,
            volumes: RowSet::empty(),
            // A pending intent is a fact about the state, not about this run's
            // candidates: a scheduled preview must report it — and so exit `3` —
            // even when the plan names nothing, because a real run of this plan
            // replays it.
            pending: preview_pending(state_paths)?,
            unreached: RowSet::empty(),
            space_stage: disabled_space_stage(),
            space_rows: None,
            space_triggered: None,
        });
    }

    // A real run acts on the preview's own plan, never on a plan this process
    // recomputes: recomputing and comparing would make any single unit's change
    // — a hold, a withdrawn claim, a marker that appeared — refuse the whole
    // run, and the design keeps such a change to the unit it affects. The
    // digest binds the bytes, the plan names the units and their identities, and
    // every unit is re-verified immediately before its effect.
    let (Some(bytes), Some(digest)) = (request.plan_bytes, request.plan_digest) else {
        return Err(Error::new(
            ErrorKind::Usage,
            "a real maintain run needs the preview's exact plan bytes (--plan <file>) and its --plan-sha256; nothing was changed",
        ));
    };
    if !unit::is_sha256(digest) {
        return Err(Error::new(
            ErrorKind::Usage,
            "the plan digest is not a SHA-256 hex string; nothing was changed",
        )
        .with_detail("plan_digest", digest.into()));
    }
    if sha256_hex(bytes) != digest {
        return Err(Error::new(
            ErrorKind::Usage,
            "the plan file's bytes do not match --plan-sha256; nothing was changed",
        )
        .with_detail("file_digest", sha256_hex(bytes).into())
        .with_detail("plan_digest", digest.into()));
    }
    let plan = read_maintain_plan(bytes)?;
    // Every embedded scratch step is checked before any effect: a step whose own
    // digest does not describe its own bytes is a refusal, not a mid-run stop, so
    // nothing has been changed when it is reported.
    for volume in &plan.volumes.rows {
        let Some(step) = &volume.scratch else {
            continue;
        };
        let described = scratch::plan_digest(&step.plan).map_err(|error| {
            Error::new(
                ErrorKind::Usage,
                format!(
                    "the plan's scratch step for volume {} cannot be read: {error}; nothing was changed",
                    volume.volume_uuid
                ),
            )
        })?;
        if described != step.plan_digest {
            return Err(Error::new(
                ErrorKind::Usage,
                format!(
                    "the plan's scratch step for volume {} does not describe its own bytes; nothing was changed",
                    volume.volume_uuid
                ),
            )
            .with_detail("volume_uuid", volume.volume_uuid.clone().into())
            .with_detail("recorded", step.plan_digest.clone().into())
            .with_detail("described", described.into()));
        }
    }
    run(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        &declarations,
        plan,
        digest.to_owned(),
        request.maintain_bounds.deadline(started),
        request,
    )
}

/// Run maintenance with an optional, non-persisted space stage.
#[allow(clippy::too_many_arguments)]
pub fn maintain_with_space(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn crate::os::SpaceFacts,
    flush: &dyn crate::os::Flush,
    request: &MaintainRequest<'_>,
    enabled: bool,
) -> Result<MaintainReport> {
    let started = Instant::now();
    let deadline = request.maintain_bounds.deadline(started);
    let mut report = maintain(state_paths, dirs, facts, mounts, observer, request)?;
    if !enabled {
        return Ok(report);
    }
    report.space_stage = "enabled".to_owned();
    if report.timed_out || report.stopped.is_some() {
        let rows: Vec<_> = report
            .plan
            .volumes
            .rows
            .iter()
            .map(|v| space_empty_row(&v.volume_uuid, "unknown_state"))
            .collect();
        let total = rows.len();
        report.space_rows = Some(RowSet::bounded(rows, total, VOLUME_ROWS));
        return Ok(report);
    }
    let mut rows = Vec::new();
    let mut triggered = 0;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        report.timed_out = true;
        let rows: Vec<_> = report
            .plan
            .volumes
            .rows
            .iter()
            .map(|v| space_empty_row(&v.volume_uuid, "unknown_state"))
            .collect();
        let total = rows.len();
        report.space_rows = Some(RowSet::bounded(rows, total, VOLUME_ROWS));
        return Ok(report);
    }
    let records = match read_registry_with_budget(state_paths, remaining) {
        Ok(records) => records,
        Err(error) => {
            report.stopped = Some(error.message().to_owned());
            return Ok(report);
        }
    };
    for planned in &report.plan.volumes.rows {
        let Some(record) = records
            .iter()
            .find(|record| record.volume_uuid.as_str() == planned.volume_uuid)
        else {
            rows.push(space_empty_row(&planned.volume_uuid, "unknown_state"));
            continue;
        };
        if planned.state == VolumeState::Absent {
            // The plan already reports the absent volume. No space stage ran.
            continue;
        }
        if record.generation != planned.record_generation || record.root_dir != planned.root_dir {
            rows.push(space_empty_row(&planned.volume_uuid, "unknown_state"));
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            report.timed_out = true;
            rows.push(space_empty_row(
                record.volume_uuid.as_str(),
                "unknown_state",
            ));
            continue;
        }
        let uuid = &record.volume_uuid;
        if record.durability == Durability::Kept {
            rows.push(space_empty_row(uuid.as_str(), "skipped_kept"));
            continue;
        }
        if record.space.is_none() {
            rows.push(space_empty_row(uuid.as_str(), "space_undeclared"));
            continue;
        }
        if request.preview {
            let preview = crate::space::plan(
                state_paths,
                dirs,
                facts,
                mounts,
                observer,
                space,
                &crate::space::SpacePruneRequest {
                    volume_uuid: uuid,
                    now_ms: request.now_ms,
                    wall: remaining.min(crate::space::SPACE_PLAN_WALL),
                    lock_wait: remaining.min(Duration::from_secs(5)),
                },
            );
            match preview {
                Ok(preview) => {
                    if preview.measurement.free_bytes <= preview.target_bytes {
                        triggered += 1;
                    }
                    rows.push(serde_json::json!({"volume_uuid": uuid, "preview": {"schema": crate::space::SPACE_PRUNE_SCHEMA, "command": "space prune", "ok": true, "data": preview}}));
                }
                Err(error) => {
                    if matches!(error.kind(), ErrorKind::UnknownState | ErrorKind::Io) {
                        report.stopped = Some(error.message().to_owned());
                    }
                    rows.push(space_error_row(uuid, &error));
                }
            }
        } else {
            let result = crate::space::decide_volume(
                state_paths,
                dirs,
                facts,
                mounts,
                observer,
                space,
                flush,
                uuid,
                remaining.min(Duration::from_secs(120)),
                false,
                crate::space::SpacePruneBounds::design(),
            );
            match result {
                Ok(decision) => rows.push(serde_json::json!({
                    "volume_uuid": uuid, "decision": decision.decision,
                    "F": decision.after.free_bytes, "R": decision.policy.reserve_bytes, "L": decision.policy.floor_bytes,
                    "measurement_source": decision.after.source,
                    "pruned_root_ids": decision.pruned.rows.iter().map(|row| &row.root_id).collect::<Vec<_>>(),
                    "replayed_root_ids": decision.detail["replayed_root_ids"],
                    "replay_partial_root_ids": decision.detail["replay_partial_root_ids"],
                    "replay_settled_root_ids": decision.detail["replay_settled_root_ids"],
                    "cause": decision.detail["cause"], "owned_prunable_bytes": decision.detail["owned_prunable_bytes"],
                    "held_or_unknown_bytes": decision.detail["held_or_unknown_bytes"],
                    "excluded_bytes": decision.detail["excluded_bytes"], "other_estimate_bytes": decision.detail["other_estimate_bytes"]
                })),
                Err(error) => {
                    if matches!(error.kind(), ErrorKind::UnknownState | ErrorKind::Io) {
                        report.stopped = Some(error.message().to_owned());
                    }
                    rows.push(space_error_row(uuid, &error));
                }
            }
        }
    }
    if request.preview {
        report.space_triggered = Some(triggered);
    }
    let total = rows.len();
    report.space_rows = Some(RowSet::bounded(rows, total, VOLUME_ROWS));
    // Keep the enclosing command within its document bound. Each omitted volume
    // is counted, rather than turning an incomplete report into an empty one.
    loop {
        let size = serde_json::to_vec(&report)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("cannot encode maintenance report: {error}"),
                )
            })?
            .len();
        if size <= DOCUMENT_SIZE_CAP_BYTES.saturating_sub(1024) {
            break;
        }
        let Some(rowset) = report.space_rows.as_mut() else {
            break;
        };
        if rowset.rows.pop().is_none() {
            break;
        }
        rowset.count = rowset.rows.len();
        rowset.truncated = rowset.total > rowset.count;
    }
    Ok(report)
}

fn space_empty_row(uuid: &str, decision: &str) -> serde_json::Value {
    serde_json::json!({"volume_uuid": uuid, "decision": decision, "F": null, "R": null, "L": null,
        "measurement_source": null, "pruned_root_ids": null, "replayed_root_ids": null,
        "replay_partial_root_ids": null, "replay_settled_root_ids": null, "cause": null,
        "owned_prunable_bytes": null, "held_or_unknown_bytes": null, "excluded_bytes": null,
        "other_estimate_bytes": null})
}

fn space_error_row(uuid: &crate::os::VolumeUuid, error: &Error) -> serde_json::Value {
    let detail = error.detail();
    let decision = if error.kind() == ErrorKind::SpaceBelowFloor {
        "space_below_floor"
    } else {
        "unknown_state"
    };
    serde_json::json!({
        "volume_uuid": uuid, "decision": decision, "error_kind": error.kind().as_str(), "message": error.message(),
        "F": detail.get("free_bytes"), "R": detail.get("reserve_bytes"), "L": detail.get("floor_bytes"),
        "measurement_source": detail.get("measurement_source"),
        "pruned_root_ids": detail.get("pruned_root_ids"), "replayed_root_ids": detail.get("replayed_root_ids"),
        "replay_partial_root_ids": detail.get("replay_partial_root_ids"),
        "replay_settled_root_ids": detail.get("replay_settled_root_ids"), "cause": detail.get("cause"),
        "owned_prunable_bytes": detail.get("owned_prunable_bytes"),
        "held_or_unknown_bytes": detail.get("held_or_unknown_bytes"),
        "excluded_bytes": detail.get("excluded_bytes"), "other_estimate_bytes": detail.get("other_estimate_bytes")
    })
}

/// Read the registry under the shared observation lock. Nothing is created.
fn read_registry(state_paths: &StatePaths) -> Result<Vec<VolumeRecord>> {
    let mut locks = state::LockSet::new(state_paths.clone());
    let _lock = locks.observe_registry()?;
    Ok(registry::read(state_paths)?.volumes().to_vec())
}

fn read_registry_with_budget(
    state_paths: &StatePaths,
    budget: Duration,
) -> Result<Vec<VolumeRecord>> {
    let deadline = state::Deadline::after(budget.min(state::READ_DEADLINE));
    let mut locks = state::LockSet::with_deadline(state_paths.clone(), deadline);
    let _lock = locks.observe_registry()?;
    Ok(registry::read_with_deadline(state_paths, deadline)?
        .volumes()
        .to_vec())
}

/// The plan's canonical bytes: one compact JSON document, exactly the bytes the
/// plan digest covers and a real run must present.
pub fn canonical_plan_bytes(plan: &MaintainPlan) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(plan).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!(
                "could not encode the {} plan: {error}",
                MAINTAIN_PLAN_SCHEMA
            ),
        )
    })?;
    if bytes.len() > DOCUMENT_SIZE_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the {} plan is {} bytes, over the {DOCUMENT_SIZE_CAP_BYTES}-byte document cap",
                MAINTAIN_PLAN_SCHEMA,
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", DOCUMENT_SIZE_CAP_BYTES.into()));
    }
    Ok(bytes)
}

/// Build the whole plan, in registry order and then declaration order.
#[allow(clippy::too_many_arguments)]
fn build_plan(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    records: &[VolumeRecord],
    declarations: &area::DeclarationSet,
    request: &MaintainRequest<'_>,
) -> Result<MaintainPlan> {
    let builder = Builder {
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        declarations,
        sweep_bounds: request.sweep_bounds,
        delete_bounds: request.delete_bounds,
        now_ms: request.now_ms,
    };
    let mut rows = Vec::new();
    for record in records {
        rows.push(builder.volume_plan(record)?);
    }
    let total = rows.len();
    Ok(MaintainPlan {
        schema: MAINTAIN_PLAN_SCHEMA.to_owned(),
        policy: SweepPolicy::design(),
        volumes: RowSet::bounded(rows, total, VOLUME_ROWS),
    })
}

/// The read-only half of a maintain run: everything the plan can say.
struct Builder<'a> {
    state_paths: &'a StatePaths,
    dirs: &'a dyn DirOps,
    facts: &'a dyn AbsenceVolume,
    mounts: &'a dyn MountRoot,
    observer: &'a dyn ProcessObserver,
    declarations: &'a area::DeclarationSet,
    sweep_bounds: SweepBounds,
    delete_bounds: DeleteBounds,
    now_ms: u64,
}

impl Builder<'_> {
    /// One volume's step, or the reason it has none.
    fn volume_plan(&self, record: &VolumeRecord) -> Result<VolumePlan> {
        let mut row = VolumePlan {
            volume_uuid: record.volume_uuid.as_str().to_owned(),
            availability: record.availability,
            durability: record.durability,
            mount_hint: record.mount_hint.clone(),
            root_dir: record.root_dir.clone(),
            record_generation: record.generation,
            state: VolumeState::Present,
            detail: None,
            loud: false,
            scratch: None,
            areas: RowSet::empty(),
        };
        if let Err(error) = area::VolumeRoot::open(self.dirs, self.facts, self.mounts, record) {
            if error.kind() != ErrorKind::VolumeAbsent {
                return Err(error);
            }
            // An absent volume is a report. Its recorded mount point stays
            // untouched: a stale mount-point directory is never written to, and
            // nothing is recreated.
            row.state = VolumeState::Absent;
            row.loud = record.availability == Availability::Permanent;
            row.detail = Some(if row.loud {
                format!(
                    "permanent volume {} is not attached; its scratch and areas were left alone and its mount point was not recreated",
                    record.volume_uuid
                )
            } else {
                format!(
                    "ephemeral volume {} is not attached; it was skipped",
                    record.volume_uuid
                )
            });
            return Ok(row);
        }
        let durability = match record.declared_durability() {
            Ok(durability) => durability,
            Err(error) => {
                row.loud = true;
                row.detail = Some(format!(
                    "volume {} has no declared durability, so nothing on it is pruned: {error}",
                    record.volume_uuid
                ));
                return Ok(row);
            }
        };
        if !durability.permits_pruning() {
            row.detail = Some(format!(
                "volume {} is declared `{}`, so nothing on it is pruned",
                record.volume_uuid,
                durability_name(durability)
            ));
            return Ok(row);
        }

        let sweep = scratch::sweep(
            self.state_paths,
            self.dirs,
            self.facts,
            self.mounts,
            self.observer,
            &SweepRequest {
                volume_uuid: &record.volume_uuid,
                preview: true,
                plan_digest: None,
                bounds: self.sweep_bounds,
                delete_bounds: self.delete_bounds,
                now_ms: self.now_ms,
            },
        )?;
        row.scratch = Some(ScratchStep {
            plan_digest: sweep.report.plan_digest,
            plan: sweep.report.plan,
        });

        let mut area_rows = Vec::new();
        for document in &self.declarations.documents {
            if let Some(step) = self.area_plan(record, document)? {
                area_rows.push(step);
            }
        }
        let area_total = area_rows.len();
        row.areas = RowSet::bounded(area_rows, area_total, AREA_ROWS);
        Ok(row)
    }

    /// One declared area's prune step on one volume, or `None` when the area
    /// declares no prune policy (it is then not a maintain step at all).
    fn area_plan(
        &self,
        record: &VolumeRecord,
        document: &AreaDocument,
    ) -> Result<Option<AreaPrunePlan>> {
        let declaration = document.declaration();
        let Some(age_seconds) = prune_age_seconds(&declaration.prune) else {
            return Ok(None);
        };
        let resolved = area::resolve(
            self.dirs,
            self.facts,
            self.mounts,
            self.observer,
            record,
            &declaration,
            Some(document),
        )?;
        // The age this step decides is the stage 4 rule: a bounded scan of the
        // root's subtree, never the marker's creation time alone.
        let listing = area::list_roots_with(
            self.dirs,
            self.observer,
            self.state_paths,
            &resolved,
            ROOT_ROWS,
            area::ActivityMode::Scan,
        )?;
        let age_ms = age_seconds.saturating_mul(1000);
        let mut candidates = Vec::new();
        let mut kept = Vec::new();
        for root in &listing.roots {
            let path = Some(root.configured_path.clone());
            if root.state != MarkerStateName::Owned {
                kept.push(PruneKeptRow {
                    name: root.name.clone(),
                    path,
                    state: marker_state_name(root.state).to_owned(),
                    reason: "the entry's marker is not an owned root, so no prune policy covers it"
                        .to_owned(),
                });
                continue;
            }
            let (Some(device), Some(inode), Some(digest), Some(kind), Some(last)) = (
                root.device,
                root.inode,
                root.marker_digest.clone(),
                root.kind,
                root.last_activity_ms,
            ) else {
                kept.push(PruneKeptRow {
                    name: root.name.clone(),
                    path,
                    state: marker_state_name(root.state).to_owned(),
                    reason: "the root's identity or last activity is unreadable".to_owned(),
                });
                continue;
            };
            if !root.idle {
                kept.push(PruneKeptRow {
                    name: root.name.clone(),
                    path,
                    state: marker_state_name(root.state).to_owned(),
                    reason: "a live or undecidable hold covers the root".to_owned(),
                });
                continue;
            }
            // The comparison is evaluated against the plan's own clock, and the
            // row's reason never states the elapsed time: that value moves every
            // second, and a plan whose bytes move cannot be the authorization a
            // real run presents. The decision is re-evaluated against the effect's
            // clock immediately before the deletion instead.
            if self.now_ms.saturating_sub(last) < age_ms {
                kept.push(PruneKeptRow {
                    name: root.name.clone(),
                    path,
                    state: marker_state_name(root.state).to_owned(),
                    reason: format!("idle for less than the declared {age_seconds} s"),
                });
                continue;
            }
            candidates.push(PruneRootRow {
                name: root.name.clone(),
                path: root.configured_path.clone(),
                device,
                inode,
                marker_digest: digest,
                kind,
                created_ms: root.created_ms,
                last_activity_ms: last,
                reason: format!("owned and idle past the declared {age_seconds} s"),
            });
        }
        let candidate_total = candidates.len();
        let kept_total = kept.len();
        Ok(Some(AreaPrunePlan {
            namespace: document.namespace.clone(),
            area: document.area.clone(),
            folder: resolved.folder.clone(),
            policy: declaration.prune.clone(),
            candidates: RowSet::bounded(candidates, candidate_total, ROOT_ROWS),
            kept: RowSet::bounded(kept, kept_total, ROOT_ROWS),
            examined: listing.examined,
            truncated: listing.truncated,
        }))
    }
}

/// The declared prune age, when the policy has one.
fn prune_age_seconds(policy: &PrunePolicy) -> Option<u64> {
    match policy {
        PrunePolicy::None => None,
        PrunePolicy::OldestIdleAfter { age_seconds } => Some(*age_seconds),
    }
}

/// A durability's name, as a report spells it.
const fn durability_name(durability: Durability) -> &'static str {
    match durability {
        Durability::Expendable => "expendable",
        Durability::Kept => "kept",
        Durability::Undeclared => "undeclared",
    }
}

/// A marker state's name, as a report spells it.
const fn marker_state_name(state: MarkerStateName) -> &'static str {
    match state {
        MarkerStateName::Owned => "owned",
        MarkerStateName::Absent => "absent",
        MarkerStateName::Unknown => "unknown",
    }
}

/// Read one `barge.maintain.plan/1` document from its own bytes.
///
/// A reader refuses an unknown major, a document over the size cap, a body that
/// does not carry exactly this major's field set, and a list whose counts
/// disagree with its rows. An extra field is refused, because the plan is the
/// authorization a real run presents and a document whose meaning a reader would
/// have to guess at is not one.
pub fn read_maintain_plan(bytes: &[u8]) -> Result<MaintainPlan> {
    let document = crate::contract::parse_document(bytes)?;
    let found = document
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                "the plan has no schema string, so it is not a maintain plan",
            )
        })?;
    if found != MAINTAIN_PLAN_SCHEMA {
        let prefix = "barge.maintain.plan/";
        let message = if found.starts_with(prefix) {
            format!("`{found}` is an unknown major; this reader knows {MAINTAIN_PLAN_SCHEMA}")
        } else {
            format!("`{found}` is not a {prefix} document")
        };
        return Err(Error::new(ErrorKind::UnknownState, message)
            .with_detail("schema", found.into())
            .with_detail("supported", MAINTAIN_PLAN_SCHEMA.into()));
    }
    let plan: MaintainPlan = serde_json::from_value(document).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "the plan does not provide the field set {MAINTAIN_PLAN_SCHEMA} requires: {error}"
            ),
        )
    })?;
    plan.volumes.validate()?;
    for volume in &plan.volumes.rows {
        volume.areas.validate()?;
        if let Some(step) = &volume.scratch {
            step.plan.candidates.validate()?;
            step.plan.kept.validate()?;
        }
        for area_plan in &volume.areas.rows {
            area_plan.candidates.validate()?;
            area_plan.kept.validate()?;
        }
    }
    Ok(plan)
}

/// The real half: apply the plan stage by stage, re-verifying before each effect.
#[allow(clippy::too_many_arguments)]
fn run(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    declarations: &area::DeclarationSet,
    plan: MaintainPlan,
    digest: String,
    deadline: Instant,
    request: &MaintainRequest<'_>,
) -> Result<MaintainReport> {
    let mut volumes = Vec::new();
    let mut pending: Vec<PendingRow> = Vec::new();
    let mut unreached: Vec<UnreachedRow> = Vec::new();
    let mut timed_out = false;
    let mut stopped: Option<String> = None;
    let candidates = plan.candidate_count();

    'volumes: for volume_plan in &plan.volumes.rows {
        let mut outcome = VolumeOutcome {
            volume_uuid: volume_plan.volume_uuid.clone(),
            state: volume_plan.state,
            loud: volume_plan.loud,
            detail: volume_plan.detail.clone(),
            scratch: None,
            areas: Vec::new(),
        };
        if !volume_plan.has_work() {
            volumes.push(outcome);
            continue;
        }
        // The plan's volume must still be the registered one, with the folder it
        // planned against. The registry is re-read here, before this volume's
        // stage, rather than trusted from the run's opening read: a revision that
        // landed after the run started must not authorise a stale record. A record
        // that left the registry or moved its `root_dir` keeps every unit the plan
        // named for it, as that volume only, and the other volumes still run.
        // A registry that cannot be read again is a mid-run failure: earlier
        // volumes may already have been maintained, so it is reported as a stop
        // (exit `3`) with this volume's units kept, never an error document that
        // would drop the work already done.
        let fresh = match read_registry(state_paths) {
            Ok(fresh) => fresh,
            Err(error) => {
                let reason = format!(
                    "the registry could not be read again before volume {}'s stage: {error}",
                    volume_plan.volume_uuid
                );
                fill_kept(&mut outcome, volume_plan, &reason, &mut unreached, true);
                stopped = Some(reason);
                volumes.push(outcome);
                break 'volumes;
            }
        };
        let Some(record) = fresh
            .iter()
            .find(|record| record.volume_uuid.as_str() == volume_plan.volume_uuid)
        else {
            fill_unmaintainable(
                &mut outcome,
                volume_plan,
                &format!(
                    "volume {} left the registry between the plan and the effect; every planned unit it names is kept",
                    volume_plan.volume_uuid
                ),
                true,
            );
            volumes.push(outcome);
            continue;
        };
        if record.root_dir != volume_plan.root_dir {
            fill_unmaintainable(
                &mut outcome,
                volume_plan,
                &format!(
                    "the volume's root_dir changed since the plan (from `{}` to `{}`); every planned unit it names is kept",
                    volume_plan.root_dir, record.root_dir
                ),
                true,
            );
            volumes.push(outcome);
            continue;
        }
        // The plan is the authorization, so the record it was made under must still
        // be the record this run acts under. Durability is decided first, because a
        // volume that became `undeclared` needs a human (exit `3`) while one
        // declared `kept` does not; any such change also advances the generation,
        // so the generation branch below would otherwise report both quietly.
        if !record.durability.permits_pruning() {
            fill_kept(
                &mut outcome,
                volume_plan,
                &format!(
                    "the volume is now declared `{}`, which never permits pruning; every planned unit it names is kept",
                    durability_name(record.durability)
                ),
                &mut unreached,
                record.durability == Durability::Undeclared,
            );
            volumes.push(outcome);
            continue;
        }
        if record.generation != volume_plan.record_generation {
            fill_kept(
                &mut outcome,
                volume_plan,
                &format!(
                    "the volume record changed since the plan (generation {} to {}); every planned unit it names is kept",
                    volume_plan.record_generation, record.generation
                ),
                &mut unreached,
                false,
            );
            volumes.push(outcome);
            continue;
        }

        if let Some(step) = &volume_plan.scratch {
            if Instant::now() >= deadline {
                timed_out = true;
                unreached.push(UnreachedRow {
                    volume_uuid: volume_plan.volume_uuid.clone(),
                    stage: "scratch".to_owned(),
                    candidates: step.plan.candidates.count,
                });
                fill_unreached_areas(
                    &mut outcome,
                    &volume_plan.volume_uuid,
                    &volume_plan.areas.rows,
                    &mut unreached,
                    "the run's wall clock was reached before this area",
                );
                volumes.push(outcome);
                break 'volumes;
            }
            // The run's deadline is passed into the stage, so every candidate's
            // effect is gated by it: a stage with many units cannot outlive the
            // run's bound and then be reported as a success.
            let run = match scratch::expire_plan(
                state_paths,
                dirs,
                facts,
                mounts,
                observer,
                record,
                &step.plan,
                volume_plan.record_generation,
                delete::DeleteBounds {
                    deadline: Some(deadline),
                    ..request.delete_bounds
                },
                request.now_ms,
                Some(deadline),
            ) {
                Ok(run) => run,
                Err(error) => {
                    // Work may already have been done on an earlier volume, so
                    // this is never a "refused before any effect" refusal. The
                    // volume that stopped is reported in full: every unit the plan
                    // named for it is kept with the reason and counted as unreached.
                    let reason = format!(
                        "the scratch stage of volume {} stopped the run: {error}",
                        record.volume_uuid
                    );
                    fill_kept(
                        &mut outcome,
                        volume_plan,
                        &reason,
                        &mut unreached,
                        is_loud(volume_plan),
                    );
                    stopped = Some(reason);
                    volumes.push(outcome);
                    break 'volumes;
                }
            };
            if run.timed_out {
                timed_out = true;
            }
            // A stage that stopped mid-way keeps the rows it earned: the units it
            // deleted, the intents it left pending and the unit that failed.
            let stage_stopped = run.stopped.clone();
            if run.unreached > 0 {
                unreached.push(UnreachedRow {
                    volume_uuid: volume_plan.volume_uuid.clone(),
                    stage: "scratch".to_owned(),
                    candidates: run.unreached,
                });
            }
            pending.extend(run.report.pending.rows.iter().cloned());
            outcome.scratch = Some(ScratchOutcome {
                plan_digest: run.report.plan_digest,
                deleted: run.report.deleted,
                pending: run.report.pending,
                skipped: run.report.skipped,
                replay: run.report.replay,
            });
            if let Some(reason) = stage_stopped {
                // The stage stopped on a failure that is not one unit's refusal.
                // Its own rows stay in the report; the areas it never reached are
                // kept, and the run stops here with exit 3.
                outcome.detail = Some(reason.clone());
                stopped = Some(reason);
                fill_remaining_areas(
                    &mut outcome,
                    volume_plan,
                    "the run stopped before this area, so the planned unit is kept",
                );
                volumes.push(outcome);
                break 'volumes;
            }
            if timed_out {
                // No further effect may start. The areas of this volume the run
                // did not reach are reported, with counts, and the other volumes
                // are reported as unreached below.
                fill_unreached_areas(
                    &mut outcome,
                    &volume_plan.volume_uuid,
                    &volume_plan.areas.rows,
                    &mut unreached,
                    "the run's wall clock was reached before this area",
                );
                volumes.push(outcome);
                break 'volumes;
            }
        }

        // A volume that went away between the plan and this run keeps every unit
        // the plan named for it, reports why, and leaves the other volumes alone.
        // Any other failure here is mid-run: earlier volumes — and this volume's
        // scratch stage — may already have been maintained, so it is a stop
        // reported in `data.stopped` (exit `3`), never a refusal document.
        if let Err(error) = area::VolumeRoot::open(dirs, facts, mounts, record) {
            if error.kind() != ErrorKind::VolumeAbsent {
                fill_kept(
                    &mut outcome,
                    volume_plan,
                    &format!(
                        "volume {} could not be verified again before its areas; every unit the plan named for it that was not already handled is kept ({error})",
                        record.volume_uuid
                    ),
                    &mut unreached,
                    is_loud(volume_plan),
                );
                stopped = outcome.detail.clone();
                volumes.push(outcome);
                break 'volumes;
            }
            fill_unmaintainable(
                &mut outcome,
                volume_plan,
                &format!(
                    "volume {} is not attached any more; every planned unit it names is kept",
                    record.volume_uuid
                ),
                is_loud(volume_plan),
            );
            volumes.push(outcome);
            continue;
        }

        for area_plan in &volume_plan.areas.rows {
            let mut stage = AreaPruneOutcome::empty(&area_plan.namespace, &area_plan.area);
            if area_plan.candidates.count == 0 {
                outcome.areas.push(stage);
                continue;
            }
            let Some(document) = declarations.find(&area_plan.namespace, &area_plan.area) else {
                stage.skipped = all_skipped(
                    area_plan,
                    "the declaration was withdrawn between the plan and the effect",
                );
                outcome.areas.push(stage);
                continue;
            };
            let declaration = document.declaration();
            // The comparison is against the policy the preview saved, never
            // against a value derived from the current declaration: a declaration
            // edited between the preview and the effect must not decide the age
            // this plan's units are judged by.
            if prune_age_seconds(&area_plan.policy).is_none() {
                stage.skipped = all_skipped(
                    area_plan,
                    "the plan's step declares no prune policy, so no unit is acted on",
                );
                outcome.areas.push(stage);
                continue;
            }
            if declaration.prune != area_plan.policy {
                stage.skipped = all_skipped(
                    area_plan,
                    "the declaration's prune policy changed since the plan; the planned units are kept",
                );
                outcome.areas.push(stage);
                continue;
            }
            // A folder override that moved the area between the plan and the
            // effect keeps this area's units: the plan named paths the current
            // configuration no longer resolves.
            let folder = match area::effective_folder(record, &declaration) {
                Ok(folder) => folder,
                Err(error) => {
                    // A stored declaration is validated when the run loads it, so a
                    // refusal here means the record changed underneath this run. It
                    // is still after earlier effects, so it is a stop (exit 3), not a
                    // refusal document: this area and every stage after it are kept.
                    stage.skipped = all_skipped(
                        area_plan,
                        &format!("the area's folder could not be resolved: {error}"),
                    );
                    outcome.areas.push(stage);
                    let reason = format!(
                        "the area {namespace}.{area} stopped the run: {error}",
                        namespace = area_plan.namespace,
                        area = area_plan.area,
                    );
                    outcome.detail = Some(reason.clone());
                    stopped = Some(reason);
                    fill_remaining_areas(
                        &mut outcome,
                        volume_plan,
                        "the run stopped before this area, so the planned unit is kept",
                    );
                    volumes.push(outcome);
                    break 'volumes;
                }
            };
            if folder != area_plan.folder {
                stage.skipped = all_skipped(
                    area_plan,
                    "the area's folder changed since the plan; the planned units are kept",
                );
                outcome.areas.push(stage);
                continue;
            }
            let mut deleted = Vec::new();
            let mut skipped = Vec::new();
            let mut stage_pending: Vec<PendingRow> = Vec::new();
            for candidate in &area_plan.candidates.rows {
                if let Some(reason) = &stopped {
                    // Every planned unit is accounted for: a unit the stop left
                    // unreached is reported with counts, never dropped.
                    skipped.push(AreaSkippedRow {
                        name: candidate.name.clone(),
                        reason: format!("the run stopped before this effect: {reason}"),
                    });
                    continue;
                }
                if !stage_pending.is_empty() {
                    skipped.push(AreaSkippedRow {
                        name: candidate.name.clone(),
                        reason:
                            "a pending intent stopped this area's prune; a later run replays it"
                                .to_owned(),
                    });
                    continue;
                }
                if Instant::now() >= deadline {
                    timed_out = true;
                    skipped.push(AreaSkippedRow {
                        name: candidate.name.clone(),
                        reason: "the run's wall clock was reached before this effect".to_owned(),
                    });
                    continue;
                }
                let bounds = delete::DeleteBounds {
                    deadline: Some(deadline),
                    ..request.delete_bounds
                };
                let unit = area_unit(record, area_plan, candidate, &digest);
                // The per-unit re-verification runs inside the deletion and can be
                // slow, so the clock is checked here at the effect boundary: the
                // deletion never starts inside the stop window.
                if Instant::now() >= deadline {
                    timed_out = true;
                    skipped.push(AreaSkippedRow {
                        name: candidate.name.clone(),
                        reason: "the run's wall clock was reached before this effect".to_owned(),
                    });
                    continue;
                }
                // The deletion resolves the declaration a caller presents against
                // the state directory's own record, so present that record as it now
                // stands. A declaration edited after this step began is then judged
                // by the per-unit re-verification below, which keeps the unit under
                // the plan's own policy instead of the call being refused as stale.
                // A folder that moved keeps the unit exactly as the step-start check
                // decides, and an absent or unreadable record presents the step's
                // declaration and leaves the decision to the deletion's own read.
                let presented = match area::show(state_paths, &area_plan.namespace, &area_plan.area)
                {
                    Ok(Some(document)) => {
                        let current = document.declaration();
                        match area::effective_folder(record, &current) {
                            Ok(folder) if folder == area_plan.folder => current,
                            _ => {
                                skipped.push(AreaSkippedRow {
                                    name: candidate.name.clone(),
                                    reason: "the area's folder changed since the plan; the planned units are kept"
                                        .to_owned(),
                                });
                                continue;
                            }
                        }
                    }
                    Ok(None) | Err(_) => declaration.clone(),
                };
                let authorize = |fresh: &VolumeRecord| {
                    reverify_area_root(
                        state_paths,
                        dirs,
                        facts,
                        mounts,
                        observer,
                        fresh,
                        volume_plan.record_generation,
                        document,
                        candidate,
                        &area_plan.policy,
                        request.now_ms,
                    )
                };
                match delete::delete_unit_authorized(
                    state_paths,
                    dirs,
                    facts,
                    mounts,
                    observer,
                    &presented,
                    &unit,
                    bounds,
                    Some(&authorize),
                    &crate::state::IntentEvidence::none(),
                ) {
                    Ok(report) => {
                        if report.is_deleted() {
                            deleted.push(AreaDeletedRow {
                                name: candidate.name.clone(),
                                path: candidate.path.clone(),
                                intent_id: report.intent_id,
                                entries_removed: report.entries_removed,
                                directories_removed: report.directories_removed,
                                marker_removed: report.marker_removed,
                            });
                        } else {
                            stage_pending.push(PendingRow {
                                intent_id: report.intent_id,
                                unit_name: report.unit_name,
                                unit_kind: report.unit_kind,
                                phase: report.phase,
                                reason: report
                                    .reason
                                    .unwrap_or_else(|| "the deletion stopped short".to_owned()),
                            });
                        }
                    }
                    Err(error) if scratch::is_per_unit_refusal(&error) => {
                        skipped.push(AreaSkippedRow {
                            name: candidate.name.clone(),
                            reason: error.to_string(),
                        });
                    }
                    Err(error) => {
                        skipped.push(AreaSkippedRow {
                            name: candidate.name.clone(),
                            reason: format!("the run stopped on this unit's error: {error}"),
                        });
                        outcome.detail = Some(format!(
                            "the area stage {}/{} stopped the run: {error}",
                            area_plan.namespace, area_plan.area
                        ));
                        stopped = outcome.detail.clone();
                    }
                }
                // An attempt that ended past the clock — deleted, pending or
                // refused — is still an overrun, so no later effect starts and the
                // run reports the timeout.
                if Instant::now() >= deadline {
                    timed_out = true;
                }
            }
            pending.extend(stage_pending.iter().cloned());
            let deleted_total = deleted.len();
            let skipped_total = skipped.len();
            let stage_pending_total = stage_pending.len();
            stage.deleted = RowSet::bounded(deleted, deleted_total, ROOT_ROWS);
            stage.skipped = RowSet::bounded(skipped, skipped_total, ROOT_ROWS);
            stage.pending = RowSet::bounded(stage_pending, stage_pending_total, ROOT_ROWS);
            outcome.areas.push(stage);
            if stopped.is_some() || timed_out {
                // The run cannot continue: the remaining areas of this volume are
                // reported with counts, and the later volumes below. A stop names
                // its own stage; the clock names every stage it did not reach.
                let index = volume_plan
                    .areas
                    .rows
                    .iter()
                    .position(|plan| {
                        plan.namespace == area_plan.namespace && plan.area == area_plan.area
                    })
                    .map_or(volume_plan.areas.rows.len(), |index| index + 1);
                let reason = if timed_out {
                    "the run's wall clock was reached before this area"
                } else {
                    "the run stopped before this area"
                };
                fill_unreached_areas(
                    &mut outcome,
                    &volume_plan.volume_uuid,
                    &volume_plan.areas.rows[index.min(volume_plan.areas.rows.len())..],
                    &mut unreached,
                    reason,
                );
                volumes.push(outcome);
                break 'volumes;
            }
        }
        // A record that became `undeclared` while the run was deleting is only
        // visible to the per-unit refusals; the volume's own report is loud for it,
        // because expending data is never inferred. An `undeclared` record keeps
        // its units, and that needs a human rather than a quiet partial run.
        if area::registered_volume(state_paths, &record.volume_uuid)
            .is_ok_and(|current| current.durability == Durability::Undeclared)
        {
            outcome.loud = true;
            if outcome.detail.is_none() {
                outcome.detail = Some(
                    "the volume's durability is now undeclared, so the units the plan named are kept"
                        .to_owned(),
                );
            }
        }
        volumes.push(outcome);
    }
    // A run that crossed its own clock at any point — including in its last
    // effect or re-verification — is a timeout, never a success.
    if Instant::now() >= deadline {
        timed_out = true;
    }
    if timed_out || stopped.is_some() {
        // Report what the run did not reach, with counts, so the unfinished part
        // is never reported as done.
        for volume_plan in &plan.volumes.rows {
            if volumes
                .iter()
                .any(|outcome| outcome.volume_uuid == volume_plan.volume_uuid)
            {
                continue;
            }
            for (stage, candidates) in unreached_stage_rows(volume_plan) {
                unreached.push(UnreachedRow {
                    volume_uuid: volume_plan.volume_uuid.clone(),
                    stage,
                    candidates,
                });
            }
        }
    }

    let pending_total = pending.len();
    let unreached_total = unreached.len();
    let volume_total = volumes.len();
    Ok(MaintainReport {
        preview: false,
        plan_digest: digest,
        planned_ms: request.now_ms,
        timed_out,
        stopped,
        candidates,
        plan,
        volumes: RowSet::bounded(volumes, volume_total, VOLUME_ROWS),
        pending: RowSet::bounded(pending, pending_total, ROOT_ROWS),
        unreached: RowSet::bounded(unreached, unreached_total, ROOT_ROWS),
        space_stage: disabled_space_stage(),
        space_rows: None,
        space_triggered: None,
    })
}

/// The stages of one volume plan the run did not reach, with the candidate count
/// each one would have attempted, so a stopped run reports counts and never
/// presents the unfinished part as done.
fn unreached_stage_rows(volume_plan: &VolumePlan) -> Vec<(String, usize)> {
    let mut stages = Vec::new();
    if let Some(step) = &volume_plan.scratch {
        stages.push(("scratch".to_owned(), step.plan.candidates.count));
    }
    for area_plan in &volume_plan.areas.rows {
        stages.push((
            format!("area:{}/{}", area_plan.namespace, area_plan.area),
            area_plan.candidates.count,
        ));
    }
    stages
}

/// Report one volume's area stages as unreached: every planned unit is kept with
/// the reason, and each stage's candidate count is recorded, so a run the wall
/// clock stopped never presents the unfinished part as done.
fn fill_unreached_areas(
    outcome: &mut VolumeOutcome,
    volume_uuid: &str,
    remaining: &[AreaPrunePlan],
    unreached: &mut Vec<UnreachedRow>,
    reason: &str,
) {
    for area_plan in remaining {
        if area_plan.candidates.count > 0 {
            unreached.push(UnreachedRow {
                volume_uuid: volume_uuid.to_owned(),
                stage: format!("area:{}/{}", area_plan.namespace, area_plan.area),
                candidates: area_plan.candidates.count,
            });
        }
        outcome.areas.push(AreaPruneOutcome {
            namespace: area_plan.namespace.clone(),
            area: area_plan.area.clone(),
            deleted: RowSet::empty(),
            skipped: all_skipped(area_plan, reason),
            pending: RowSet::empty(),
        });
    }
}

/// Report a whole volume's planned units as kept, with one reason: the plan is
/// the authorization, so a volume whose record left the registry, whose
/// `root_dir` moved or which is no longer attached can have none of its units
/// acted on. The volume is then loud, because the run did not do what it planned
/// for it, and the other volumes still run.
fn fill_unmaintainable(
    outcome: &mut VolumeOutcome,
    volume_plan: &VolumePlan,
    reason: &str,
    loud: bool,
) {
    outcome.detail = Some(reason.to_owned());
    outcome.loud = loud;

    // A stage that already ran keeps its own rows: a mid-run absence must never
    // restate units it deleted as kept.
    if outcome.scratch.is_some() {
        fill_remaining_areas(outcome, volume_plan, reason);
        return;
    }
    if let Some(step) = &volume_plan.scratch {
        let skipped: Vec<scratch::SkippedRow> = step
            .plan
            .candidates
            .rows
            .iter()
            .map(|row| scratch::SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: reason.to_owned(),
            })
            .collect();
        let total = skipped.len();
        outcome.scratch = Some(ScratchOutcome {
            plan_digest: step.plan_digest.clone(),
            deleted: RowSet::empty(),
            pending: RowSet::empty(),
            skipped: RowSet::bounded(skipped, total, ROOT_ROWS),
            replay: None,
        });
    }
    outcome.areas = volume_plan
        .areas
        .rows
        .iter()
        .map(|area_plan| AreaPruneOutcome {
            namespace: area_plan.namespace.clone(),
            area: area_plan.area.clone(),
            deleted: RowSet::empty(),
            skipped: all_skipped(area_plan, reason),
            pending: RowSet::empty(),
        })
        .collect();
}

/// Every planned unit of one volume, kept with one reason and never loud: a record
/// that changed or a durability that no longer permits pruning is an operator
/// change, so the units are kept and reported, not escalated.
fn fill_kept(
    outcome: &mut VolumeOutcome,
    volume_plan: &VolumePlan,
    reason: &str,
    unreached: &mut Vec<UnreachedRow>,
    loud: bool,
) {
    outcome.detail = Some(reason.to_owned());
    outcome.loud = loud;
    if outcome.scratch.is_some() {
        fill_remaining_areas(outcome, volume_plan, reason);
        return;
    }
    if let Some(step) = &volume_plan.scratch {
        let skipped: Vec<scratch::SkippedRow> = step
            .plan
            .candidates
            .rows
            .iter()
            .map(|row| scratch::SkippedRow {
                name: row.name.clone(),
                scope: row.scope.clone(),
                reason: reason.to_owned(),
            })
            .collect();
        let total = skipped.len();
        if total > 0 {
            unreached.push(UnreachedRow {
                volume_uuid: volume_plan.volume_uuid.clone(),
                stage: "scratch".to_owned(),
                candidates: total,
            });
        }
        outcome.scratch = Some(ScratchOutcome {
            plan_digest: step.plan_digest.clone(),
            deleted: RowSet::empty(),
            pending: RowSet::empty(),
            skipped: RowSet::bounded(skipped, total, ROOT_ROWS),
            replay: None,
        });
    }
    outcome.areas = volume_plan
        .areas
        .rows
        .iter()
        .map(|area_plan| AreaPruneOutcome {
            namespace: area_plan.namespace.clone(),
            area: area_plan.area.clone(),
            deleted: RowSet::empty(),
            skipped: all_skipped(area_plan, reason),
            pending: RowSet::empty(),
        })
        .collect();
}

/// Whether a report about one volume needs a human: only a permanent volume's
/// absence is loud, because a drive that should be attached and is not needs one.
/// An ephemeral volume that is away — at preview or mid-run — is reported quietly.
const fn is_loud(volume_plan: &VolumePlan) -> bool {
    volume_plan.loud || matches!(volume_plan.availability, Availability::Permanent)
}

/// Append kept rows for the area stages this volume has not reported yet, so a
/// mid-run stop never duplicates or rewrites a stage that already ran.
fn fill_remaining_areas(outcome: &mut VolumeOutcome, volume_plan: &VolumePlan, reason: &str) {
    let reported: Vec<(String, String)> = outcome
        .areas
        .iter()
        .map(|stage| (stage.namespace.clone(), stage.area.clone()))
        .collect();
    for area_plan in &volume_plan.areas.rows {
        if reported
            .iter()
            .any(|(namespace, area)| namespace == &area_plan.namespace && area == &area_plan.area)
        {
            continue;
        }
        outcome.areas.push(AreaPruneOutcome {
            namespace: area_plan.namespace.clone(),
            area: area_plan.area.clone(),
            deleted: RowSet::empty(),
            skipped: all_skipped(area_plan, reason),
            pending: RowSet::empty(),
        });
    }
}

/// Every planned candidate of one area, kept with one reason.
fn all_skipped(area_plan: &AreaPrunePlan, reason: &str) -> RowSet<AreaSkippedRow> {
    let rows: Vec<AreaSkippedRow> = area_plan
        .candidates
        .rows
        .iter()
        .map(|candidate| AreaSkippedRow {
            name: candidate.name.clone(),
            reason: reason.to_owned(),
        })
        .collect();
    let total = rows.len();
    RowSet::bounded(rows, total, ROOT_ROWS)
}

/// The deletion unit one planned area root is.
fn area_unit(
    record: &VolumeRecord,
    area_plan: &AreaPrunePlan,
    candidate: &PruneRootRow,
    digest: &str,
) -> DeletionUnit {
    DeletionUnit {
        volume_uuid: record.volume_uuid.clone(),
        root_dir: record.root_dir.clone(),
        folder: area_plan.folder.clone(),
        parent: String::new(),
        name: candidate.name.clone(),
        root_id: None,
        kind: UnitKind::Directory,
        device: DeviceId::new(candidate.device),
        inode: candidate.inode,
        marker: Some(UnitMarker::new(
            MarkerFamily::Barge,
            candidate.marker_digest.clone(),
        )),
        grant: UnitGrant::native_root(candidate.name.clone()),
        plan_digest: digest.to_owned(),
    }
}

/// Re-verify one planned area root inside the deletion's own window.
///
/// This runs under the volume lock, after the registry record was re-read and
/// before the intent is written: the declaration must still declare the same
/// prune age, the root must still be that identity with that marker bytes
/// digest, it must still be idle, and its idleness must still be past the
/// declared age. Every refusal here is per-unit, so a changed root or a
/// withdrawn policy stops only that root.
#[allow(clippy::too_many_arguments)]
fn reverify_area_root(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    planned_generation: u64,
    document: &AreaDocument,
    candidate: &PruneRootRow,
    planned_policy: &PrunePolicy,
    now_ms: u64,
) -> Result<()> {
    if record.generation != planned_generation {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the registry record of volume {} changed between the plan and the effect (generation {} to {}); the root is kept",
                record.volume_uuid, planned_generation, record.generation
            ),
        ));
    }
    let Some(stored) = area::show(state_paths, &document.namespace, &document.area)? else {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the declaration {}/{} was withdrawn between the plan and the effect; the root is kept",
                document.namespace, document.area
            ),
        ));
    };
    let declaration = stored.declaration();
    // Re-read under the effect lock: the current declaration must still declare
    // exactly the policy the preview saved, so a policy edited in this run's own
    // window cannot decide the age the plan's units are judged by.
    let Some(age_seconds) = prune_age_seconds(planned_policy) else {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the plan's step for {}/{} declares no prune policy; the root is kept",
                stored.namespace, stored.area
            ),
        ));
    };
    if &declaration.prune != planned_policy {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the prune policy of {}/{} changed since the plan; the root is kept",
                stored.namespace, stored.area
            ),
        ));
    }
    let resolved = area::resolve(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &declaration,
        Some(&stored),
    )?;
    let listing = area::list_roots_with(
        dirs,
        observer,
        state_paths,
        &resolved,
        ROOT_ROWS,
        area::ActivityMode::Scan,
    )?;
    let Some(observed) = listing
        .roots
        .iter()
        .find(|root| root.root_id.as_deref() == Some(candidate.name.as_str()))
    else {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the root {} is no longer an owned root of {}/{}; it is kept",
                candidate.name, stored.namespace, stored.area
            ),
        ));
    };
    if observed.state != MarkerStateName::Owned
        || observed.device != Some(candidate.device)
        || observed.inode != Some(candidate.inode)
        || observed.marker_digest.as_deref() != Some(candidate.marker_digest.as_str())
    {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the root {}'s identity or marker changed between the plan and the effect; it is kept",
                candidate.name
            ),
        ));
    }
    if !observed.idle {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "a live or undecidable hold now covers the root {}; it is kept",
                candidate.name
            ),
        ));
    }
    let last = observed.last_activity_ms.unwrap_or(now_ms);
    if now_ms.saturating_sub(last) < age_seconds.saturating_mul(1000) {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the root {} is no longer idle past the declared {age_seconds} s; it is kept",
                candidate.name
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        canonical_plan_bytes, fill_unmaintainable, maintain, MaintainBounds, MaintainReport,
        MaintainRequest, ScratchOutcome, VolumeOutcome, VolumeState,
    };
    use crate::area::{
        self, ConclusionClosure, ConclusionOutcome, KeptReason, NestedScope, OwnedRoot,
        PrunePolicy, RootKind, RootSpec,
    };
    use crate::contract::{RowSet, INTENT_ROWS, ROOT_ROWS};
    use crate::delete::DeleteBounds;
    use crate::digest::sha256_hex;
    use crate::error::ErrorKind;
    use crate::registry::{self, Availability, ClaimScope, ClaimUnit, Durability, ScratchClaim};
    use crate::scratch::{self, AllocRequest, ConcludeRequest, SweepBounds};
    use crate::state;
    use crate::testkit::{Faults, VolumeFixture, FIXTURE_BOOT};

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const AREA_AGE_SECONDS: u64 = 60;

    /// A fixture volume with one declared area whose prune age is `60` seconds.
    struct Harness {
        fixture: VolumeFixture,
        now_ms: u64,
    }

    impl Harness {
        fn new(label: &str) -> Self {
            let fixture = VolumeFixture::new(label);
            fixture.register();
            let now_ms = state::now_ms();
            let harness = Self { fixture, now_ms };
            harness.declare();
            harness
        }

        fn declare(&self) {
            area::declare(&self.fixture.state, &self.declaration(), self.now_ms)
                .expect("the area is declared");
        }

        fn declaration(&self) -> area::AreaDeclaration {
            let mut declaration = self.fixture.declaration("pool");
            declaration.prune = PrunePolicy::OldestIdleAfter {
                age_seconds: AREA_AGE_SECONDS,
            };
            declaration
        }

        /// A "now" far enough past the roots' activity for the declared age.
        fn aged(&self) -> u64 {
            self.fixture.state_biased_now()
        }

        fn create_root(&self, kind: RootKind) -> OwnedRoot {
            let resolved = self
                .fixture
                .resolved(&self.fixture.record, &self.declaration());
            area::create_root(
                &self.fixture.state,
                &self.fixture.platform(),
                &self.fixture.observer(),
                &resolved,
                RootSpec { kind },
            )
            .expect("the root is created")
        }

        fn request<'a>(
            &self,
            preview: bool,
            plan_digest: Option<&'a str>,
            plan_bytes: Option<&'a [u8]>,
            now_ms: u64,
        ) -> MaintainRequest<'a> {
            self.request_with_bounds(
                preview,
                plan_digest,
                plan_bytes,
                now_ms,
                MaintainBounds::design(),
            )
        }

        fn request_with_bounds<'a>(
            &self,
            preview: bool,
            plan_digest: Option<&'a str>,
            plan_bytes: Option<&'a [u8]>,
            now_ms: u64,
            maintain_bounds: MaintainBounds,
        ) -> MaintainRequest<'a> {
            MaintainRequest {
                preview,
                plan_digest,
                plan_bytes,
                sweep_bounds: SweepBounds::default(),
                delete_bounds: DeleteBounds {
                    retry_pause: Duration::ZERO,
                    ..DeleteBounds::default()
                },
                maintain_bounds,
                now_ms,
            }
        }

        fn preview(&self) -> MaintainReport {
            self.preview_at(self.aged())
        }

        fn preview_at(&self, now_ms: u64) -> MaintainReport {
            let facts = self.fixture.facts();
            self.preview_with(&facts, now_ms)
        }

        fn preview_with(&self, facts: &crate::testkit::FakeVolume, now_ms: u64) -> MaintainReport {
            let platform = self.fixture.platform();
            maintain(
                &self.fixture.state,
                &platform,
                facts,
                &self.fixture.mounts(),
                &self.fixture.observer(),
                &self.request(true, None, None, now_ms),
            )
            .expect("the plan is computed")
        }

        /// The real form the host job uses: the preview's own bytes and digest.
        fn real(&self, preview: &MaintainReport) -> crate::error::Result<MaintainReport> {
            self.real_with(&self.fixture.platform(), self.aged(), preview)
        }

        fn real_with(
            &self,
            dirs: &dyn crate::os::DirOps,
            now_ms: u64,
            preview: &MaintainReport,
        ) -> crate::error::Result<MaintainReport> {
            self.real_request(dirs, now_ms, &preview.plan_digest, &canonical(preview))
        }

        /// The real form with a caller's own wall clock, so a test can state a
        /// run whose deadline is already spent or spent by one effect.
        fn real_with_bounds(
            &self,
            dirs: &dyn crate::os::DirOps,
            now_ms: u64,
            preview: &MaintainReport,
            maintain_bounds: MaintainBounds,
        ) -> crate::error::Result<MaintainReport> {
            maintain(
                &self.fixture.state,
                dirs,
                &self.fixture.facts(),
                &self.fixture.mounts(),
                &self.fixture.observer(),
                &self.request_with_bounds(
                    false,
                    Some(&preview.plan_digest),
                    Some(&canonical(preview)),
                    now_ms,
                    maintain_bounds,
                ),
            )
        }

        fn real_with_facts(
            &self,
            facts: &crate::testkit::FakeVolume,
            preview: &MaintainReport,
        ) -> crate::error::Result<MaintainReport> {
            let platform = self.fixture.platform();
            maintain(
                &self.fixture.state,
                &platform,
                facts,
                &self.fixture.mounts(),
                &self.fixture.observer(),
                &self.request(
                    false,
                    Some(&preview.plan_digest),
                    Some(&canonical(preview)),
                    self.aged(),
                ),
            )
        }

        /// One real request with explicit bytes and digest, so a test can state
        /// a tampered plan or a digest that is not the bytes' own.
        fn real_request(
            &self,
            dirs: &dyn crate::os::DirOps,
            now_ms: u64,
            digest: &str,
            bytes: &[u8],
        ) -> crate::error::Result<MaintainReport> {
            maintain(
                &self.fixture.state,
                dirs,
                &self.fixture.facts(),
                &self.fixture.mounts(),
                &self.fixture.observer(),
                &self.request(false, Some(digest), Some(bytes), now_ms),
            )
        }

        fn area_plan<'a>(&self, report: &'a MaintainReport) -> &'a super::AreaPrunePlan {
            &report.plan.volumes.rows[0].areas.rows[0]
        }

        fn scratch_root(&self, project: &str, consumer: &str) -> PathBuf {
            let platform = self.fixture.platform();
            let report = scratch::alloc(
                &self.fixture.state,
                &platform,
                &self.fixture.facts(),
                &self.fixture.mounts(),
                &self.fixture.observer(),
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

        fn conclude_and_release(&self, path: &Path, holder: &str) {
            let platform = self.fixture.platform();
            let root_id = path
                .file_name()
                .expect("a root name")
                .to_string_lossy()
                .to_string();
            scratch::conclude(
                &self.fixture.state,
                &platform,
                &self.fixture.facts(),
                &self.fixture.mounts(),
                &self.fixture.observer(),
                &ConcludeRequest {
                    volume_uuid: &self.fixture.uuid,
                    root_id: &root_id,
                    holder,
                    project: None,
                    outcome: ConclusionOutcome::Closed,
                    closure: ConclusionClosure::Positive,
                    kept_reason: KeptReason::Evidence,
                    nested_scope: NestedScope::Exclusive,
                },
            )
            .expect("the run concludes");
            area::release(
                &self.fixture.state,
                &self.fixture.observer(),
                &root_id,
                holder,
            )
            .expect("the hold is released");
        }

        fn scratch_folder(&self) -> PathBuf {
            self.fixture
                .root
                .join(scratch::scratch_folder(&self.fixture.record))
        }

        /// Write one live hold record under a unit name, as another process does
        /// between a preview and its real run.
        fn write_live_hold(&self, unit_name: &str, holder: &str) {
            let directory = self.fixture.state.holds_dir().join(unit_name);
            std::fs::create_dir_all(&directory).expect("the holds directory");
            let path = directory.join(format!("{holder}.json"));
            std::fs::write(
                &path,
                VolumeFixture::hold_record(
                    unit_name,
                    holder,
                    std::process::id(),
                    7_000_000,
                    FIXTURE_BOOT,
                ),
            )
            .expect("the hold is written");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("the hold is private");
        }
    }

    impl VolumeFixture {
        /// A "now" that makes this fixture's roots idle past the declared area
        /// age.
        fn state_biased_now(&self) -> u64 {
            state::now_ms() + 7 * DAY_MS + AREA_AGE_SECONDS * 1000
        }
    }

    fn canonical(report: &MaintainReport) -> Vec<u8> {
        canonical_plan_bytes(&report.plan).expect("the plan encodes")
    }

    #[test]
    fn an_effect_revalidation_uses_the_roots_own_fresh_time() {
        // A root planned as idle is touched after the preview so its own directory now
        // says a time ahead of the plan's clock, while an earlier listing still says the
        // old one. The effect-time revalidation reads the root's own fresh time, so the
        // age is unknown and the root is kept; an implementation that preferred the
        // stale listing would delete it.
        let harness = Harness::new("maintain-effect-fresh-time");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert!(
            preview.plan.volumes.rows.iter().any(|volume| volume
                .areas
                .rows
                .iter()
                .any(|area| area.candidates.count == 1)),
            "the root is planned as idle: {:?}",
            preview.plan
        );

        // The root is touched at a time ahead of the plan's clock, and every listing
        // keeps reporting the old time.
        let plan_now = harness.aged();
        let touched = plan_now + 2 * DAY_MS;
        let directory = std::fs::File::open(&root.path).expect("the root directory opens");
        directory
            .set_modified(std::time::UNIX_EPOCH + Duration::from_millis(touched))
            .expect("the root's modification time is set");
        drop(directory);
        let dirs = crate::testkit::Faults::default();
        dirs.listing_the_modified_time_as(harness.fixture.state_biased_now() - 8 * DAY_MS);
        let report = harness
            .real_with(&dirs, plan_now, &preview)
            .expect("the real run completes");
        assert_eq!(report.volumes.rows.len(), 1);
        let deleted: usize = report
            .volumes
            .rows
            .iter()
            .map(|volume| {
                volume
                    .areas
                    .iter()
                    .map(|area| area.deleted.count)
                    .sum::<usize>()
            })
            .sum();
        assert_eq!(deleted, 0, "the touched root was deleted: {report:?}");
        let skipped: usize = report
            .volumes
            .rows
            .iter()
            .map(|volume| {
                volume
                    .areas
                    .iter()
                    .map(|area| area.skipped.count)
                    .sum::<usize>()
            })
            .sum();
        assert_eq!(skipped, 1, "the root is reported as kept: {report:?}");
        assert!(root.path.is_dir(), "the root is still there");
    }

    #[test]
    fn a_preview_plans_scratch_then_each_areas_policy_and_changes_nothing() {
        let harness = Harness::new("maintain-preview");
        // One concluded, released scratch run: eligible through S4's policy.
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        // Two owned area roots: one idle past the declared age, one held live.
        let idle = harness.create_root(RootKind::AreaRoot);
        let held = harness.create_root(RootKind::AreaRoot);
        harness.write_live_hold(&held.root_id, "someone");

        let report = harness.preview();
        assert!(report.preview);
        assert_eq!(report.plan.volumes.count, 1);
        let volume = &report.plan.volumes.rows[0];
        assert_eq!(volume.state, VolumeState::Present);
        assert!(!volume.loud);
        let scratch_step = volume.scratch.as_ref().expect("a scratch stage");
        assert_eq!(scratch_step.plan.candidates.count, 1);
        let area_plan = harness.area_plan(&report);
        assert_eq!(area_plan.candidates.count, 1);
        assert_eq!(area_plan.candidates.rows[0].name, idle.root_id);
        assert_eq!(area_plan.kept.count, 1);
        assert_eq!(area_plan.kept.rows[0].name, held.root_id);
        assert!(area_plan.kept.rows[0].reason.contains("hold"));
        assert_eq!(report.candidates, 2);
        assert!(!report.needs_human());
        // A preview changes nothing: both roots and the scratch run are there,
        // and no intent was written.
        assert!(idle.path.is_dir());
        assert!(held.path.is_dir());
        assert!(scratch_root.is_dir());
        assert!(
            !harness.fixture.state.intents_dir().exists()
                || std::fs::read_dir(harness.fixture.state.intents_dir())
                    .expect("the intents directory")
                    .next()
                    .is_none()
        );
    }

    #[test]
    fn the_plan_digest_is_the_sha256_of_the_canonical_plan_and_is_stable() {
        let harness = Harness::new("maintain-digest");
        let _root = harness.create_root(RootKind::AreaRoot);
        let first = harness.preview();
        let second = harness.preview();
        assert_eq!(first.plan_digest, second.plan_digest, "a preview is stable");
        assert_eq!(first.plan, second.plan);
        assert_eq!(first.plan_digest, sha256_hex(&canonical(&first)));
        let document: serde_json::Value =
            serde_json::from_slice(&canonical(&first)).expect("the plan parses");
        assert_eq!(document["schema"], "barge.maintain.plan/1");
    }

    #[test]
    fn the_plan_bytes_are_stable_across_elapsed_seconds_and_authorize_the_real_run() {
        let harness = Harness::new("maintain-plan-stable");
        let root = harness.create_root(RootKind::AreaRoot);
        let planned_at = harness.aged();
        let first = harness.preview_at(planned_at);
        let first_candidate = &harness.area_plan(&first).candidates.rows[0];
        // The candidate states the declared policy, never how long it has been
        // idle: that number moves every second, and a plan whose bytes move can
        // never be the authorization a real run presents.
        assert_eq!(
            first_candidate.reason,
            format!("owned and idle past the declared {AREA_AGE_SECONDS} s")
        );

        // Five seconds later the same state produces the same bytes and digest.
        let second = harness.preview_at(planned_at + 5_000);
        assert_eq!(second.plan_digest, first.plan_digest);
        assert_eq!(second.plan, first.plan);

        // A real run a minute after the preview still accepts that preview's
        // digest, and re-evaluates the root's age against its own clock before
        // the effect.
        let report = harness
            .real_request(
                &harness.fixture.platform(),
                planned_at + 60_000,
                &first.plan_digest,
                &canonical(&first),
            )
            .expect("the preview's digest authorizes the run");
        assert_eq!(report.volumes.rows[0].areas[0].deleted.count, 1);
        assert!(!root.path.exists(), "the planned root is gone");
    }

    #[test]
    fn a_fresh_root_s_reason_states_the_policy_and_never_the_elapsed_time() {
        let harness = Harness::new("maintain-kept-reason-stable");
        let _root = harness.create_root(RootKind::AreaRoot);
        let first = harness.preview_at(harness.now_ms);
        let second = harness.preview_at(harness.now_ms + 5_000);
        let reason =
            |report: &MaintainReport| harness.area_plan(report).kept.rows[0].reason.clone();
        assert_eq!(
            reason(&first),
            format!("idle for less than the declared {AREA_AGE_SECONDS} s")
        );
        assert_eq!(reason(&first), reason(&second));
        assert_eq!(first.plan_digest, second.plan_digest);
        assert_eq!(harness.area_plan(&first).candidates.count, 0);
    }

    #[test]
    fn a_real_run_deletes_exactly_the_planned_roots() {
        let harness = Harness::new("maintain-real");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let idle = harness.create_root(RootKind::AreaRoot);
        let held = harness.create_root(RootKind::AreaRoot);
        harness.write_live_hold(&held.root_id, "someone");

        let preview = harness.preview();
        let report = harness.real(&preview).expect("the run reports");
        assert!(!report.preview);
        assert_eq!(report.plan_digest, preview.plan_digest);
        assert!(!report.needs_human(), "{report:?}");
        let volume = &report.volumes.rows[0];
        let scratch_stage = volume.scratch.as_ref().expect("a scratch stage");
        assert_eq!(scratch_stage.deleted.count, 1);
        assert_eq!(scratch_stage.pending.count, 0);
        let area_stage = &volume.areas[0];
        assert_eq!(area_stage.deleted.count, 1);
        assert_eq!(area_stage.deleted.rows[0].name, idle.root_id);
        assert!(area_stage.deleted.rows[0].marker_removed);
        assert!(!idle.path.exists(), "the planned root is gone");
        assert!(held.path.is_dir(), "the held root is kept");
        assert!(!scratch_root.exists(), "the planned scratch run is gone");
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_changed_plan_digest_refuses_before_any_effect() {
        let harness = Harness::new("maintain-digest-mismatch");
        let idle = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let error = harness
            .real_request(
                &harness.fixture.platform(),
                harness.aged(),
                &"0".repeat(64),
                &canonical(&preview),
            )
            .expect_err("a digest that is not the plan's own refuses");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("do not match"));
        assert!(idle.path.is_dir(), "nothing was deleted");
    }

    #[test]
    fn a_real_run_without_the_preview_s_bytes_or_digest_is_a_usage_refusal() {
        let harness = Harness::new("maintain-no-digest");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let platform = harness.fixture.platform();
        // Neither the bytes nor the digest: nothing to authorize.
        let error = maintain(
            &harness.fixture.state,
            &platform,
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(false, None, None, harness.aged()),
        )
        .expect_err("a real run needs the preview's bytes and digest");
        assert_eq!(error.kind(), ErrorKind::Usage);
        // The digest without the bytes: the plan's units are unknown, so the run
        // refuses rather than recomputing a plan of its own.
        let error = maintain(
            &harness.fixture.state,
            &platform,
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(false, Some(&preview.plan_digest), None, harness.aged()),
        )
        .expect_err("a digest alone does not name the plan's units");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("exact plan bytes"));
        // The bytes without the digest: nothing binds them.
        let bytes = canonical(&preview);
        let error = maintain(
            &harness.fixture.state,
            &platform,
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(false, None, Some(&bytes), harness.aged()),
        )
        .expect_err("bytes alone are not an authorization");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(root.path.is_dir(), "nothing was deleted");
    }

    #[test]
    fn plan_bytes_that_do_not_match_the_digest_refuse() {
        let harness = Harness::new("maintain-tampered-bytes");
        let idle = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let mut tampered = canonical(&preview);
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        let error = harness
            .real_request(
                &harness.fixture.platform(),
                harness.aged(),
                &preview.plan_digest,
                &tampered,
            )
            .expect_err("a tampered plan refuses");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("do not match"));
        assert!(idle.path.is_dir());
    }

    #[test]
    fn a_plan_authorizes_only_the_units_it_names() {
        let harness = Harness::new("maintain-plan-authority");
        let planned = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 1);
        // A root created after the preview is eligible but unnamed: the plan is
        // the authorization, so a real run never grows to cover it.
        let unnamed = harness.create_root(RootKind::AreaRoot);
        let report = harness.real(&preview).expect("the run reports");
        assert_eq!(report.volumes.rows[0].areas[0].deleted.count, 1);
        assert!(!planned.path.exists(), "the planned unit went");
        assert!(unnamed.path.is_dir(), "an unnamed unit is never deleted");
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_root_that_became_marked_differently_between_the_plan_and_the_effect_is_kept() {
        let harness = Harness::new("maintain-changed-root");
        let first = harness.create_root(RootKind::AreaRoot);
        let second = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 2);

        // The probe rewrites the first root's marker the moment the deletion's
        // own window lists it, which is the plan-to-effect gap a racer lands in.
        let marker = first.path.join(area::ROOT_MARKER_FILE);
        let target = first.root_id.clone();
        let target_path = first.path.clone();
        let faults = Faults::new();
        let seen = std::cell::Cell::new(0usize);
        let marker_for_probe = marker.clone();
        faults.observing(Box::new(move |name: &str| {
            if name != target {
                return;
            }
            let count = seen.get() + 1;
            seen.set(count);
            // The first observation of the unit's own name is the deletion's
            // present check: after the plan, before its re-verification.
            if count != 1 {
                return;
            }
            let bytes = std::fs::read(&marker_for_probe).expect("the marker");
            let mut document: serde_json::Value =
                serde_json::from_slice(&bytes).expect("the marker parses");
            let created = document["created_ms"].as_u64().expect("created_ms");
            document["created_ms"] = serde_json::Value::from(created + 1);
            let mut rewritten = serde_json::to_vec(&document).expect("the marker encodes");
            rewritten.push(b'\n');
            let staged = target_path.join(".barge-marker-rewrite");
            std::fs::write(&staged, &rewritten).expect("the staged marker");
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
                .expect("the staged marker is private");
            std::fs::rename(&staged, &marker_for_probe).expect("the marker is replaced");
        }));

        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports");
        let area_stage = &report.volumes.rows[0].areas[0];
        assert_eq!(area_stage.deleted.count, 1, "{area_stage:?}");
        assert_eq!(area_stage.skipped.count, 1);
        assert_eq!(area_stage.skipped.rows[0].name, first.root_id);
        assert!(
            area_stage.skipped.rows[0]
                .reason
                .contains("identity or marker changed"),
            "{}",
            area_stage.skipped.rows[0].reason
        );
        assert!(first.path.is_dir(), "the changed root is kept");
        assert!(!second.path.exists(), "the unchanged root still goes");
    }

    #[test]
    fn a_live_hold_taken_between_the_plan_and_the_effect_stops_only_that_root() {
        let harness = Harness::new("maintain-live-hold");
        let held = harness.create_root(RootKind::AreaRoot);
        let free = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 2);

        // The hold arrives after the preview and before the real run. The plan
        // still names both roots; the held one is re-verified, found not idle,
        // and kept, and the other one goes.
        harness.write_live_hold(&held.root_id, "someone");
        let report = harness.real(&preview).expect("the run reports");
        let area_stage = &report.volumes.rows[0].areas[0];
        assert_eq!(area_stage.deleted.count, 1, "{area_stage:?}");
        assert_eq!(area_stage.skipped.count, 1);
        assert_eq!(area_stage.skipped.rows[0].name, held.root_id);
        assert!(
            area_stage.skipped.rows[0].reason.contains("hold"),
            "{}",
            area_stage.skipped.rows[0].reason
        );
        assert!(held.path.is_dir(), "a held root is kept");
        assert!(!free.path.exists(), "the unheld planned root still went");
        assert_eq!(report.pending.count, 0);
        assert!(!report.needs_human(), "{report:?}");
    }

    #[test]
    fn a_registry_revision_change_keeps_that_volume_s_planned_units() {
        let harness = Harness::new("maintain-registry-revision");
        let first = harness.create_root(RootKind::AreaRoot);
        let second = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 2);

        // The record changes between the preview and the real run. The plan
        // names a generation the live record no longer has, so every root the
        // plan named for this volume is re-verified and kept — this volume's
        // units only, and the run is not refused.
        let mut bumped = harness.fixture.record.clone();
        bumped.registered_ms = bumped.registered_ms.saturating_add(1);
        harness.fixture.register_record(&bumped);
        let report = harness.real(&preview).expect("the run reports");
        let volume = &report.volumes.rows[0];
        // A generation bump is any registry write, and the plan was made under the
        // old record: every unit it named for this volume is kept, and the reason
        // names the record rather than a per-unit identity.
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("volume record changed since the plan"),
            "{:?}",
            volume.detail
        );
        let area_stage = &volume.areas[0];
        assert_eq!(area_stage.deleted.count, 0, "{area_stage:?}");
        assert_eq!(area_stage.skipped.count, 2);
        assert!(
            area_stage
                .skipped
                .rows
                .iter()
                .all(|row| row.reason.contains("volume record changed since the plan")),
            "{:?}",
            area_stage.skipped.rows
        );
        assert!(
            first.path.is_dir() && second.path.is_dir(),
            "nothing was deleted"
        );
        assert_eq!(report.pending.count, 0);
        assert!(!report.needs_human(), "{report:?}");
    }

    #[test]
    fn a_volume_that_went_away_before_the_effect_keeps_every_planned_unit() {
        let harness = Harness::new("maintain-volume-gone");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 1);

        // The plan named the unit while the volume was attached; by the time the
        // real run starts, the volume is gone. The plan is the authorization, so
        // nothing is attempted and the volume is loud, without refusing the run.
        let facts = harness.fixture.facts();
        facts.present.set(false);
        let platform = harness.fixture.platform();
        let bytes = canonical(&preview);
        let report = maintain(
            &harness.fixture.state,
            &platform,
            &facts,
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(
                false,
                Some(&preview.plan_digest),
                Some(&bytes),
                harness.aged(),
            ),
        )
        .expect("the run reports the volume");
        let volume = &report.volumes.rows[0];
        assert!(volume.loud, "{volume:?}");
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("not attached any more"),
            "{:?}",
            volume.detail
        );
        let area_stage = &volume.areas[0];
        assert_eq!(area_stage.deleted.count, 0);
        assert_eq!(area_stage.skipped.count, 1);
        assert!(report.needs_human());
        assert!(root.path.is_dir(), "the planned unit is kept");
    }

    #[test]
    fn a_volume_whose_root_dir_moved_before_the_effect_keeps_its_units() {
        let harness = Harness::new("maintain-root-dir-moved");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let mut moved = harness.fixture.record.clone();
        moved.root_dir = "managed".to_owned();
        harness.fixture.register_record(&moved);

        let report = harness.real(&preview).expect("the run reports");
        let volume = &report.volumes.rows[0];
        assert!(volume.loud, "{volume:?}");
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("root_dir changed"),
            "{:?}",
            volume.detail
        );
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(volume.areas[0].skipped.count, 1);
        assert!(root.path.is_dir(), "the planned unit is kept");
        assert!(report.needs_human());
    }

    #[test]
    fn an_area_policy_change_between_the_plan_and_the_effect_keeps_that_area() {
        let harness = Harness::new("maintain-policy-changed");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 1);

        // The declaration's policy is edited after the preview. The plan's own
        // saved policy is the authorization, so nothing in that area is acted on
        // under the new policy — even when the new policy would also cover the
        // root — and the run still completes.
        let mut changed = harness.declaration();
        changed.prune = PrunePolicy::OldestIdleAfter {
            age_seconds: AREA_AGE_SECONDS * 2,
        };
        area::declare(&harness.fixture.state, &changed, harness.now_ms)
            .expect("the area is redeclared");
        let report = harness.real(&preview).expect("the run reports");
        let area_stage = &report.volumes.rows[0].areas[0];
        assert_eq!(area_stage.deleted.count, 0, "{area_stage:?}");
        assert_eq!(area_stage.skipped.count, 1);
        assert!(
            area_stage.skipped.rows[0]
                .reason
                .contains("prune policy changed since the plan"),
            "{}",
            area_stage.skipped.rows[0].reason
        );
        assert!(root.path.is_dir(), "the planned unit is kept");
        assert_eq!(report.pending.count, 0);
        assert!(!report.needs_human(), "{report:?}");
    }

    #[test]
    fn an_area_policy_change_inside_the_effect_window_keeps_that_area_s_units() {
        let harness = Harness::new("maintain-policy-changed-in-window");
        let first = harness.create_root(RootKind::AreaRoot);
        let second = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 2);

        // The declaration is edited inside the run's own window: the probe fires
        // at the deletion's present check, after the stage's policy comparison and
        // before the effect-lock re-verification, so the refusal comes from the
        // re-read under the lock.
        let target = first.root_id.clone();
        let state = harness.fixture.state.clone();
        let mut changed = harness.declaration();
        changed.prune = PrunePolicy::OldestIdleAfter {
            age_seconds: AREA_AGE_SECONDS * 2,
        };
        let seen = std::cell::Cell::new(0usize);
        let faults = Faults::new();
        faults.observing(Box::new(move |name: &str| {
            if name != target {
                return;
            }
            let count = seen.get() + 1;
            seen.set(count);
            if count != 1 {
                return;
            }
            area::declare(&state, &changed, state::now_ms())
                .expect("the area is redeclared inside the window");
        }));
        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports");
        let area_stage = &report.volumes.rows[0].areas[0];
        // The other root may already have gone under the planned policy before the
        // probe fired; every unit re-verified after the change is kept by the
        // effect-lock comparison, and every planned unit is accounted for.
        assert_eq!(
            area_stage.deleted.count + area_stage.skipped.count,
            2,
            "{area_stage:?}"
        );
        assert!(!area_stage.skipped.rows.is_empty());
        assert!(
            area_stage.skipped.rows.iter().all(|row| row
                .reason
                .contains("prune policy of example/pool changed since the plan")),
            "{:?}",
            area_stage.skipped.rows
        );
        for row in &area_stage.skipped.rows {
            let path = if row.name == first.root_id {
                &first.path
            } else {
                &second.path
            };
            assert!(path.is_dir(), "a kept root survives the window");
        }
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_run_whose_wall_clock_is_spent_starts_no_effect() {
        let harness = Harness::new("maintain-clock-spent");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let area_root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // A budget of zero means the deadline is already reached when the run
        // starts: nothing is attempted, every planned stage and unit is reported,
        // and the run is a timeout rather than a success.
        let report = harness
            .real_with_bounds(
                &harness.fixture.platform(),
                harness.aged(),
                &preview,
                MaintainBounds {
                    wall: Duration::ZERO,
                    stop_margin: Duration::ZERO,
                },
            )
            .expect("the run reports its timeout");
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human());
        assert_eq!(report.pending.count, 0);
        let volume = &report.volumes.rows[0];
        assert!(volume.scratch.is_none(), "no scratch effect started");
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(volume.areas[0].skipped.count, 1);
        assert!(
            volume.areas[0].skipped.rows[0]
                .reason
                .contains("wall clock"),
            "{}",
            volume.areas[0].skipped.rows[0].reason
        );
        // Every stage the clock stopped is named with its candidate count.
        assert_eq!(report.unreached.count, 2, "{:?}", report.unreached.rows);
        let stages: Vec<&str> = report
            .unreached
            .rows
            .iter()
            .map(|row| row.stage.as_str())
            .collect();
        assert!(stages.contains(&"scratch"), "{stages:?}");
        assert!(
            stages.iter().any(|stage| stage.starts_with("area:")),
            "{stages:?}"
        );
        assert!(scratch_root.is_dir() && area_root.path.is_dir());
    }

    #[test]
    fn a_deletion_that_finishes_past_the_clock_is_reported_as_the_run_s_timeout() {
        let harness = Harness::new("maintain-clock-in-flight");
        let first = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&first, "owner");
        let second = harness.scratch_root("beta", "owner");
        harness.conclude_and_release(&second, "owner");
        let preview = harness.preview();
        let stage = preview.plan.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("a scratch stage");
        assert_eq!(stage.plan.candidates.count, 2);

        // The first unit's effect outlives the run's two-second clock: the pause
        // sits inside the deletion, after the effect boundary, so the unit is
        // this run's work while the unit after it is never attempted.
        let faults = Faults::new();
        faults.sleeping_remove_prefix(crate::state::CAPTURE_PREFIX, Duration::from_millis(2_300));
        let report = harness
            .real_with_bounds(
                &faults,
                harness.aged(),
                &preview,
                MaintainBounds {
                    wall: Duration::from_secs(2),
                    stop_margin: Duration::ZERO,
                },
            )
            .expect("the run reports its timeout");
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human());
        let scratch_stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("the scratch stage ran");
        // The resumed walk is bounded by the run clock too, so a removal that
        // outlives it stops the deletion instead of finishing outside the bound: the
        // first unit's intent is left pending with the wall-clock reason and the
        // second planned unit is never attempted.
        assert_eq!(scratch_stage.deleted.count, 0, "{scratch_stage:?}");
        assert_eq!(scratch_stage.pending.count, 1, "{scratch_stage:?}");
        assert!(
            scratch_stage.pending.rows[0].reason.contains("wall clock"),
            "{}",
            scratch_stage.pending.rows[0].reason
        );
        assert_eq!(scratch_stage.skipped.count, 1);
        assert!(
            scratch_stage.skipped.rows[0]
                .reason
                .contains("pending intent"),
            "{}",
            scratch_stage.skipped.rows[0].reason
        );
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human());
        // The stop is recorded in the journal, whatever name the in-flight deletion
        // had reached, so the work it left is recoverable and reported.
        let intents = state::read_intents(&harness.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records.len(), 1, "the stop is journalled");
        assert_ne!(
            intents.records[0].phase,
            state::IntentPhase::Deleted,
            "no completion was recorded"
        );
        assert!(
            first.exists() || second.exists(),
            "the unit the stop left unstarted is still there"
        );
    }

    #[test]
    fn a_replay_that_consumes_the_clock_is_a_timeout_even_with_no_candidates() {
        let harness = Harness::new("maintain-clock-replay");
        let root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&root, "owner");
        let first = harness.preview();
        assert_eq!(first.candidates, 1);

        // A real run that stops on its leaf capture leaves a pending intent; the
        // unit's visible name is gone, so the next plan has no candidate at all.
        let capture_faults = Faults::new();
        capture_faults.failing_remove_prefix(state::CAPTURE_PREFIX, 3);
        let stopped = harness
            .real_with(&capture_faults, harness.aged(), &first)
            .expect("the run reports");
        assert!(stopped.needs_human());
        let second = harness.preview();
        assert_eq!(
            second.candidates, 0,
            "the stopped unit is a quarantine leaf"
        );

        // The next run's replay outlives a two-second clock. There is no
        // candidate to delete, so a run that reported success here would be
        // reporting an overrun as success; it must be a timeout.
        let slept = std::cell::Cell::new(false);
        let faults = Faults::new();
        faults.observing(Box::new(move |_name: &str| {
            if slept.get() {
                return;
            }
            slept.set(true);
            std::thread::sleep(Duration::from_millis(2_300));
        }));
        let report = harness
            .real_with_bounds(
                &faults,
                harness.aged(),
                &second,
                MaintainBounds {
                    wall: Duration::from_secs(2),
                    stop_margin: Duration::ZERO,
                },
            )
            .expect("the run reports its timeout");
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human(), "an overrun is never success");
        assert_eq!(report.candidates, 0);
        assert_eq!(report.unreached.count, 0, "nothing was planned");
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("the scratch stage ran");
        assert!(
            stage.replay.is_some(),
            "the replay started before the clock was reached"
        );
    }

    #[test]
    fn a_slow_reverification_stops_the_effect_at_the_run_s_wall_clock() {
        let harness = Harness::new("maintain-clock-boundary");
        let root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&root, "owner");
        let preview = harness.preview();
        assert_eq!(preview.candidates, 1);
        let name = root
            .file_name()
            .and_then(|part| part.to_str())
            .expect("a root name")
            .to_owned();

        // The unit's re-verification outlives the two-second clock. The effect
        // must not start: the check at the effect boundary keeps the unit and
        // reports it as unreached, even though re-verification found it eligible.
        let slept = std::cell::Cell::new(false);
        let faults = Faults::new();
        faults.observing(Box::new(move |observed: &str| {
            if slept.get() || observed != name {
                return;
            }
            slept.set(true);
            std::thread::sleep(Duration::from_millis(2_300));
        }));
        let report = harness
            .real_with_bounds(
                &faults,
                harness.aged(),
                &preview,
                MaintainBounds {
                    wall: Duration::from_secs(2),
                    stop_margin: Duration::ZERO,
                },
            )
            .expect("the run reports its timeout");
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human());
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("the scratch stage ran");
        assert_eq!(stage.deleted.count, 0, "{stage:?}");
        assert_eq!(stage.skipped.count, 1);
        assert!(
            stage.skipped.rows[0].reason.contains("wall clock"),
            "{}",
            stage.skipped.rows[0].reason
        );
        assert_eq!(report.unreached.count, 1);
        assert_eq!(report.unreached.rows[0].candidates, 1);
        assert!(root.is_dir(), "the unit is kept");
    }

    #[test]
    fn a_slow_reverification_that_makes_the_last_unit_ineligible_is_still_a_timeout() {
        let harness = Harness::new("maintain-clock-final-ineligible");
        let root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&root, "owner");
        let preview = harness.preview();
        assert_eq!(preview.candidates, 1);
        let name = root
            .file_name()
            .and_then(|part| part.to_str())
            .expect("a root name")
            .to_owned();
        let root_id = name.clone();
        let state = harness.fixture.state.clone();

        // The re-verification both outlives the clock and makes the unit
        // ineligible: the last candidate is skipped for its own reason, and that
        // skip must still report the overrun instead of a success.
        let slept = std::cell::Cell::new(false);
        let faults = Faults::new();
        faults.observing(Box::new(move |observed: &str| {
            if slept.get() || observed != name {
                return;
            }
            slept.set(true);
            let directory = state.holds_dir().join(&root_id);
            std::fs::create_dir_all(&directory).expect("the holds directory");
            let path = directory.join("racer.json");
            std::fs::write(
                &path,
                VolumeFixture::hold_record(
                    &root_id,
                    "racer",
                    std::process::id(),
                    7_000_000,
                    FIXTURE_BOOT,
                ),
            )
            .expect("the racer's hold");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("the hold is private");
            std::thread::sleep(Duration::from_millis(2_300));
        }));
        let report = harness
            .real_with_bounds(
                &faults,
                harness.aged(),
                &preview,
                MaintainBounds {
                    wall: Duration::from_secs(2),
                    stop_margin: Duration::ZERO,
                },
            )
            .expect("the run reports its timeout");
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human());
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("the scratch stage ran");
        assert_eq!(stage.deleted.count, 0, "{stage:?}");
        assert_eq!(stage.skipped.count, 1);
        assert!(
            stage.skipped.rows[0].reason.contains("no longer eligible"),
            "{}",
            stage.skipped.rows[0].reason
        );
        assert_eq!(
            report.unreached.count, 0,
            "no effect was refused by the clock"
        );
        assert!(root.is_dir(), "the unit is kept");
    }

    #[test]
    fn a_slow_deletion_preflight_crossing_the_clock_leaves_no_intent_and_no_effect() {
        let harness = Harness::new("maintain-clock-preflight");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 1);
        let name = root.root_id.clone();

        // The deletion's own preflight — the parent walk and the present check —
        // outlives the clock. The durable boundary inside the deletion must refuse
        // there: no intent record, no rename, and the unit kept.
        let slept = std::cell::Cell::new(false);
        let faults = Faults::new();
        faults.observing(Box::new(move |observed: &str| {
            if slept.get() || observed != name {
                return;
            }
            slept.set(true);
            std::thread::sleep(Duration::from_millis(2_300));
        }));
        let report = harness
            .real_with_bounds(
                &faults,
                harness.aged(),
                &preview,
                MaintainBounds {
                    wall: Duration::from_secs(2),
                    stop_margin: Duration::ZERO,
                },
            )
            .expect("the run reports its timeout");
        assert!(report.timed_out, "{report:?}");
        assert!(report.needs_human());
        let area_stage = &report.volumes.rows[0].areas[0];
        assert_eq!(area_stage.deleted.count, 0, "{area_stage:?}");
        assert_eq!(area_stage.skipped.count, 1);
        assert!(
            area_stage.skipped.rows[0]
                .reason
                .contains("intent was published"),
            "{}",
            area_stage.skipped.rows[0].reason
        );
        assert_eq!(
            report.pending.count, 0,
            "a refusal before the intent leaves nothing pending"
        );
        let intents = state::read_intents(&harness.fixture.state, 64).expect("the journal reads");
        assert!(
            intents.records.is_empty(),
            "no intent record was written: {:?}",
            intents.records.len()
        );
        assert!(root.path.is_dir(), "the unit is kept");
    }

    #[test]
    fn a_volume_changed_to_kept_between_the_plan_and_the_effect_prunes_nothing() {
        let harness = Harness::new("maintain-kept-after-plan");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let area_root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // The operator declares the volume kept after the preview. The plan names
        // units for an expendable volume, so none of them may be pruned: the
        // registry revision and the durability are re-checked before the effects.
        let mut record = harness.fixture.record.clone();
        record.durability = Durability::Kept;
        harness.fixture.register_record(&record);
        let report = harness.real(&preview).expect("the run reports");
        let volume = &report.volumes.rows[0];
        let stage = volume.scratch.as_ref().expect("the scratch stage reports");
        assert_eq!(stage.deleted.count, 0, "{stage:?}");
        assert_eq!(stage.skipped.count, 1);
        // A durability that no longer permits pruning is decided before the
        // generation, so the reason names the `kept` declaration.
        assert!(
            stage.skipped.rows[0]
                .reason
                .contains("never permits pruning"),
            "{}",
            stage.skipped.rows[0].reason
        );
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(volume.areas[0].skipped.count, 1);
        assert!(
            scratch_root.is_dir() && area_root.path.is_dir(),
            "nothing was pruned"
        );
        assert_eq!(report.pending.count, 0);
        assert!(!report.needs_human(), "{report:?}");
    }

    #[test]
    fn an_ephemeral_volume_that_vanishes_mid_run_stays_quiet() {
        let harness = Harness::new("maintain-ephemeral-mid-run");
        let root = harness.create_root(RootKind::AreaRoot);
        let mut record = harness.fixture.record.clone();
        record.availability = Availability::Ephemeral;
        harness.fixture.register_record(&record);
        let facts = harness.fixture.facts();
        let preview = harness.preview_with(&facts, harness.aged());
        assert_eq!(preview.candidates, 1);

        // The volume goes away between the preview and the effect. The design says
        // an absent ephemeral volume is quiet: the unit is kept and reported, and
        // the run does not need a human.
        facts.present.set(false);
        let report = harness
            .real_with_facts(&facts, &preview)
            .expect("the run reports");
        let volume = &report.volumes.rows[0];
        assert!(!volume.loud, "{volume:?}");
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("not attached any more"),
            "{:?}",
            volume.detail
        );
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(volume.areas[0].skipped.count, 1);
        assert!(!report.needs_human(), "{report:?}");
        assert!(root.path.is_dir(), "the planned unit is kept");
    }

    #[test]
    fn an_inconsistent_scratch_step_is_refused_before_any_effect() {
        let harness = Harness::new("maintain-inconsistent-step");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let mut document: serde_json::Value =
            serde_json::from_slice(&canonical(&preview)).expect("the plan parses");
        document["volumes"]["rows"][0]["scratch"]["plan_digest"] =
            serde_json::Value::String("0".repeat(64));
        let mut bytes = serde_json::to_vec(&document).expect("the plan encodes");
        bytes.push(b'\n');
        let digest = sha256_hex(&bytes);

        // The whole plan is validated before the run touches anything, so a step
        // whose own digest does not describe its own bytes is a refusal — not a
        // mid-run stop after earlier effects.
        let platform = harness.fixture.platform();
        let error = maintain(
            &harness.fixture.state,
            &platform,
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(false, Some(&digest), Some(&bytes), harness.aged()),
        )
        .expect_err("an inconsistent step refuses before any effect");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(
            error.message().contains("does not describe its own bytes"),
            "{}",
            error.message()
        );
        assert!(root.path.is_dir(), "nothing was pruned");
        let intents = state::read_intents(&harness.fixture.state, 64).expect("the journal reads");
        assert!(intents.records.is_empty(), "nothing was journalled");
    }

    #[test]
    fn a_mid_run_record_change_keeps_the_remaining_scratch_units_per_unit() {
        let harness = Harness::new("maintain-per-unit-record");
        let first = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&first, "owner");
        let second = harness.scratch_root("beta", "owner");
        harness.conclude_and_release(&second, "owner");
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // The volume is declared kept while the run is under way: the unit being
        // re-verified when the record changed is re-checked against the fresh
        // record at its own effect, and the ones after it too. The unit whose
        // effect was already authorized stays done.
        let record = harness.fixture.record.clone();
        let state = harness.fixture.state.clone();
        let mutated = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = mutated.clone();
        let names = [first.clone(), second.clone()];
        let faults = Faults::new();
        faults.observing(Box::new(move |name: &str| {
            if flag.get() {
                return;
            }
            if !names
                .iter()
                .any(|path| path.file_name().and_then(|part| part.to_str()) == Some(name))
            {
                return;
            }
            flag.set(true);
            let mut kept = record.clone();
            kept.durability = Durability::Kept;
            let mut locks = state::LockSet::with_deadline(
                state.clone(),
                state::Deadline::after(Duration::from_secs(5)),
            );
            let lock = locks.lock_registry().expect("the registry lock");
            registry::RegistryWriter::new(&state)
                .publish(&lock, &registry::RegistryDocument::new(vec![kept]))
                .expect("the registry is republished");
        }));
        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports");
        assert!(mutated.get(), "the record changed inside the run");
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("a scratch stage");
        // Every unit re-verified after the change is kept with the record reason;
        // the one already authorized may have gone.
        assert!(
            stage.skipped.count >= 1,
            "a unit after the change is kept: {stage:?}"
        );
        assert!(
            stage
                .skipped
                .rows
                .iter()
                .all(|row| row.reason.contains("does not permit pruning now")
                    || row.reason.contains("never permits pruning")
                    || row.reason.contains("record changed since the plan")),
            "{:?}",
            stage.skipped.rows
        );
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_volume_that_left_the_registry_mid_run_is_loud() {
        let harness = Harness::new("maintain-record-gone");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 1);

        // The record is removed between the preview and the effect: that is an
        // unknown volume, so it is reported loudly with its units kept.
        harness
            .fixture
            .register_record(&harness.fixture.record.clone());
        harness
            .fixture
            .state
            .initialize()
            .expect("the layout exists");
        let mut locks = state::LockSet::with_deadline(
            harness.fixture.state.clone(),
            state::Deadline::after(Duration::from_secs(5)),
        );
        let lock = locks.lock_registry().expect("the registry lock");
        registry::RegistryWriter::new(&harness.fixture.state)
            .publish(&lock, &registry::RegistryDocument::new(Vec::new()))
            .expect("an empty registry is published");
        drop(lock);
        let report = harness.real(&preview).expect("the run reports");
        let volume = &report.volumes.rows[0];
        assert!(volume.loud, "{volume:?}");
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("left the registry"),
            "{:?}",
            volume.detail
        );
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(volume.areas[0].skipped.count, 1);
        assert!(report.needs_human());
        assert!(root.path.is_dir(), "the planned unit is kept");
    }

    #[test]
    fn a_volume_declared_undeclared_between_the_plan_and_the_effect_is_loud() {
        let harness = Harness::new("maintain-undeclared-after-plan");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let area_root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // The record loses its declared durability after the preview. Expending data
        // is never inferred, so the units are kept and the volume needs a human —
        // even though the same change also advances the generation.
        let mut record = harness.fixture.record.clone();
        record.durability = Durability::Undeclared;
        harness.fixture.register_record(&record);
        let report = harness.real(&preview).expect("the run reports");
        let volume = &report.volumes.rows[0];
        assert!(volume.loud, "{volume:?}");
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("undeclared"),
            "{:?}",
            volume.detail
        );
        let stage = volume.scratch.as_ref().expect("the scratch stage reports");
        assert_eq!(stage.deleted.count, 0, "{stage:?}");
        assert_eq!(stage.skipped.count, 1);
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert!(
            scratch_root.is_dir() && area_root.path.is_dir(),
            "nothing pruned"
        );
        assert!(report.needs_human(), "{report:?}");
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_mid_run_undeclared_record_is_loud() {
        let harness = Harness::new("maintain-undeclared-mid-run");
        let first = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&first, "owner");
        let second = harness.scratch_root("beta", "owner");
        harness.conclude_and_release(&second, "owner");
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // The record loses its durability while the run is deleting: the per-unit
        // refusal keeps the remaining units, and the volume still needs a human.
        let record = harness.fixture.record.clone();
        let state = harness.fixture.state.clone();
        let mutated = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = mutated.clone();
        let names = [first.clone(), second.clone()];
        let faults = Faults::new();
        faults.observing(Box::new(move |name: &str| {
            if flag.get() {
                return;
            }
            if !names
                .iter()
                .any(|path| path.file_name().and_then(|part| part.to_str()) == Some(name))
            {
                return;
            }
            flag.set(true);
            let mut lost = record.clone();
            lost.durability = Durability::Undeclared;
            let mut locks = state::LockSet::with_deadline(
                state.clone(),
                state::Deadline::after(Duration::from_secs(5)),
            );
            let lock = locks.lock_registry().expect("the registry lock");
            registry::RegistryWriter::new(&state)
                .publish(&lock, &registry::RegistryDocument::new(vec![lost]))
                .expect("the registry is republished");
        }));
        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports");
        assert!(mutated.get(), "the record changed inside the run");
        let volume = &report.volumes.rows[0];
        assert!(volume.loud, "{volume:?}");
        assert!(
            report.needs_human(),
            "an undeclared durability needs a human"
        );
        let stage = volume.scratch.as_ref().expect("a scratch stage");
        assert!(
            stage
                .skipped
                .rows
                .iter()
                .all(|row| row.reason.contains("does not permit pruning now")
                    || row.reason.contains("never permits pruning")
                    || row.reason.contains("record changed since the plan")),
            "{:?}",
            stage.skipped.rows
        );
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_scratch_stage_that_cannot_open_reports_the_volume_s_units() {
        let harness = Harness::new("maintain-scratch-open-stops");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let area_root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // The device changes between the plan and the run, so the scratch stage
        // cannot open the volume at all. That is a mid-run failure: the volume's
        // planned units are reported as kept, with counts, and the run stops with
        // `data.stopped` (exit 3) instead of dropping the volume's accounting.
        let facts = harness.fixture.facts();
        facts.shift_device();
        let report = harness
            .real_with_facts(&facts, &preview)
            .expect("the run reports instead of refusing");
        let stopped = report.stopped.as_deref().expect("the stop is reported");
        assert!(stopped.contains("the scratch stage"), "{stopped}");
        assert!(report.needs_human());
        let volume = &report.volumes.rows[0];
        let stage = volume.scratch.as_ref().expect("the stage is reported");
        assert_eq!(stage.deleted.count, 0);
        assert_eq!(stage.skipped.count, 1, "the planned scratch unit is kept");
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(
            volume.areas[0].skipped.count, 1,
            "the planned area unit is kept"
        );
        assert!(
            report
                .unreached
                .rows
                .iter()
                .any(|row| row.stage == "scratch" && row.candidates == 1),
            "the unreached stage is counted: {:?}",
            report.unreached
        );
        assert!(scratch_root.exists(), "the kept unit is not deleted");
        assert!(area_root.path.is_dir(), "the planned root is kept");
    }

    #[test]
    fn a_scratch_stage_that_stops_keeps_the_rows_it_earned() {
        let harness = Harness::new("maintain-scratch-stage-stops");
        let first = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&first, "owner");
        let second = harness.scratch_root("beta", "owner");
        harness.conclude_and_release(&second, "owner");
        let preview = harness.preview();
        let rows = &preview.plan.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("a scratch step")
            .plan
            .candidates
            .rows;
        assert_eq!(rows.len(), 2, "two candidates are planned");
        let failing = rows[1].name.clone();

        // The second unit's effect fails in a way that is not that unit's refusal:
        // the stage stops there. The first unit's deletion is already this run's
        // work, so it stays in the report instead of being dropped with the error.
        let mut faults = Faults::new();
        faults.failing_facts_at(&failing, 1);
        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports instead of refusing");
        let stopped = report.stopped.as_deref().expect("the stop is reported");
        assert!(stopped.contains("the stage stopped"), "{stopped}");
        assert!(report.needs_human());
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("the stage is reported");
        assert_eq!(stage.deleted.count, 1, "the first deletion is kept");
        assert_eq!(stage.deleted.rows[0].name, rows[0].name);
        assert_eq!(stage.skipped.count, 1, "the failing unit is reported");
        assert_eq!(stage.skipped.rows[0].name, failing);
        assert!(!Path::new(&stage.deleted.rows[0].path).exists());
        // The area stage, which the stop never reached, is kept.
        assert!(report.volumes.rows[0]
            .areas
            .iter()
            .all(|area| area.deleted.count == 0));
    }

    #[test]
    fn a_mid_run_volume_change_is_a_stop_not_a_refusal() {
        let harness = Harness::new("maintain-volume-change-mid-run");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let area_root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 2);

        // The volume changes identity as the scratch deletion renames its unit, so
        // the scratch stage stops with a pending intent and the *area* stage's
        // re-verification then fails. That is mid-run: the run must report a stop in
        // `data.stopped` (exit 3), never a refusal document.
        let facts = harness.fixture.facts_shared();
        let faults = Faults::new().shifting_volume(facts.clone());
        let report = maintain(
            &harness.fixture.state,
            &faults,
            &*facts,
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(
                false,
                Some(&preview.plan_digest),
                Some(&canonical(&preview)),
                harness.aged(),
            ),
        )
        .expect("the run reports instead of refusing");
        let stopped = report.stopped.as_deref().expect("the stop is reported");
        assert!(stopped.contains("could not be verified again"), "{stopped}");
        assert!(report.needs_human());
        assert_eq!(report.volumes.count, 1);
        // The area stage never ran, so its planned unit is kept.
        let areas = &report.volumes.rows[0].areas;
        assert!(
            areas.iter().all(|area| area.deleted.count == 0),
            "{areas:?}"
        );
        assert!(area_root.path.is_dir(), "the planned root is kept");
    }

    #[test]
    fn a_mid_run_absence_never_restates_a_scratch_stage_that_already_ran() {
        // The helper is the fix's boundary: a stage that already deleted a unit must
        // keep its own rows when absence is reported afterwards.
        let harness = Harness::new("maintain-absence-preserves-stage");
        let preview = harness.preview();
        let volume_plan = &preview.plan.volumes.rows[0];
        let mut outcome = VolumeOutcome {
            volume_uuid: volume_plan.volume_uuid.clone(),
            state: VolumeState::Present,
            loud: false,
            detail: None,
            scratch: Some(ScratchOutcome {
                plan_digest: "a".repeat(64),
                deleted: RowSet::bounded(
                    vec![scratch::DeletedRow {
                        name: "deleted-unit".to_owned(),
                        scope: scratch::UnitScope::TopLevel,
                        family: scratch::UnitFamily::Native,
                        path: PathBuf::from("/deleted-unit"),
                        intent_id: "1".repeat(32),
                        entries_removed: 1,
                        directories_removed: 2,
                        marker_removed: true,
                    }],
                    1,
                    ROOT_ROWS,
                ),
                pending: RowSet::empty(),
                skipped: RowSet::empty(),
                replay: None,
            }),
            areas: Vec::new(),
        };
        fill_unmaintainable(
            &mut outcome,
            volume_plan,
            "volume is not attached any more",
            true,
        );
        let stage = outcome.scratch.as_ref().expect("the stage is kept");
        assert_eq!(stage.deleted.count, 1, "the deletion is still reported");
        assert_eq!(stage.deleted.rows[0].name, "deleted-unit");
        assert!(outcome.loud);
        assert_eq!(
            outcome.detail.as_deref(),
            Some("volume is not attached any more")
        );
        // The area stage, which never ran, is reported as kept.
        assert_eq!(outcome.areas.len(), volume_plan.areas.rows.len());
        assert!(outcome
            .areas
            .iter()
            .all(|area| area.deleted.count == 0 && area.skipped.count <= 1));
        // Calling it again never duplicates a stage.
        fill_unmaintainable(
            &mut outcome,
            volume_plan,
            "volume is not attached any more",
            true,
        );
        assert_eq!(outcome.areas.len(), volume_plan.areas.rows.len());
    }

    #[test]
    fn an_unreadable_registry_mid_run_is_a_stop_that_keeps_the_rows() {
        let harness = Harness::new("maintain-registry-unreadable");
        let area_root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(preview.candidates, 1);

        // The registry becomes unreadable before the run reaches its volume. That
        // is a mid-run failure — earlier volumes may already have been maintained —
        // so the run must report a stop with this volume's units kept, not refuse
        // with an error document that drops the accounting.
        std::fs::write(
            harness.fixture.state.registry_path(),
            b"{ this is not a registry",
        )
        .expect("the registry is damaged");
        let declarations =
            crate::area::read_declarations(&harness.fixture.state).expect("the declarations");
        let bytes = canonical(&preview);
        let request = harness.request(
            false,
            Some(&preview.plan_digest),
            Some(&bytes),
            harness.aged(),
        );
        let report = super::run(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &declarations,
            preview.plan.clone(),
            preview.plan_digest.clone(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            &request,
        )
        .expect("the run reports instead of refusing");
        let stopped = report.stopped.as_deref().expect("the stop is reported");
        assert!(stopped.contains("could not be read again"), "{stopped}");
        assert!(report.needs_human());
        let volume = &report.volumes.rows[0];
        assert!(volume.loud, "an unreadable registry needs a human");
        assert_eq!(volume.areas[0].deleted.count, 0);
        assert_eq!(volume.areas[0].skipped.count, 1, "the planned unit is kept");
        assert!(area_root.path.is_dir(), "nothing was deleted");
    }

    #[test]
    fn a_preview_reports_a_pending_intent_it_will_not_replay() {
        let harness = Harness::new("maintain-preview-pending");
        let root = harness.create_root(RootKind::AreaRoot);
        std::os::unix::fs::symlink("/elsewhere", root.path.join("link")).expect("the link");
        let preview = harness.preview();
        let first = harness.real(&preview).expect("the run reports");
        assert_eq!(first.pending.count, 1);

        // A scheduled run previews and nothing else. The intent an earlier run left
        // pending is a fact about the state, so the preview names it and the
        // document forces exit 3 — even though this preview changes nothing and
        // replays nothing.
        let second = harness.preview();
        assert!(second.preview);
        assert_eq!(second.pending.count, 1, "{second:?}");
        assert_eq!(
            second.pending.rows[0].intent_id,
            first.pending.rows[0].intent_id
        );
        assert!(second.needs_human(), "{second:?}");
        // The plan still names no effect: the pending intent is settled by a real
        // run, not by the preview's document.
        assert_eq!(second.candidates, 0, "{second:?}");
        let intents = state::read_intents(&harness.fixture.state, INTENT_ROWS)
            .expect("the intents are readable");
        assert_eq!(intents.records.len(), 1, "the preview journalled nothing");
        assert_eq!(intents.records[0].phase, first.pending.rows[0].phase);
    }

    #[test]
    fn a_pending_intent_makes_the_run_need_a_human() {
        let harness = Harness::new("maintain-pending");
        let root = harness.create_root(RootKind::AreaRoot);
        std::os::unix::fs::symlink("/elsewhere", root.path.join("link")).expect("the link");
        let preview = harness.preview();
        let report = harness.real(&preview).expect("the run reports");
        assert!(report.needs_human());
        assert_eq!(report.pending.count, 1);
        assert_eq!(report.volumes.rows[0].areas[0].pending.count, 1);
        assert!(
            !root.path.exists(),
            "the unit was renamed into its quarantine leaf"
        );
        let leaves = std::fs::read_dir(harness.fixture.root.join("pool"))
            .expect("the area folder")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".barge-prune-")
            })
            .count();
        assert_eq!(leaves, 1, "a stopped deletion keeps its quarantine leaf");
    }

    #[test]
    fn an_ephemeral_volume_that_is_absent_is_skipped_quietly() {
        let harness = Harness::new("maintain-ephemeral-absent");
        let mut record = harness.fixture.record.clone();
        record.availability = Availability::Ephemeral;
        harness.fixture.register_record(&record);
        let root = harness.create_root(RootKind::AreaRoot);
        let facts = harness.fixture.facts();
        facts.present.set(false);

        let report = harness.preview_with(&facts, harness.aged());
        let volume = &report.plan.volumes.rows[0];
        assert_eq!(volume.state, VolumeState::Absent);
        assert!(!volume.loud);
        assert!(!volume.has_work());
        assert!(volume
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("skipped"));
        assert!(!report.needs_human());
        assert!(root.path.is_dir());
        assert!(
            !harness.fixture.root.join("scratch").exists(),
            "an absent volume's mount point is never written"
        );
    }

    #[test]
    fn a_permanent_volume_that_is_absent_is_reported_loudly_and_needs_a_human() {
        let harness = Harness::new("maintain-permanent-absent");
        let root = harness.create_root(RootKind::AreaRoot);
        let facts = harness.fixture.facts();
        facts.present.set(false);

        let report = harness.preview_with(&facts, harness.aged());
        let volume = &report.plan.volumes.rows[0];
        assert_eq!(volume.state, VolumeState::Absent);
        assert!(volume.loud);
        assert!(volume
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("permanent volume"));
        assert!(report.needs_human(), "a permanent absence needs a human");
        assert!(root.path.is_dir());

        // The real run refuses nothing and deletes nothing: it reports the same
        // absence and keeps everything.
        let real = harness
            .real_with_facts(&facts, &report)
            .expect("the run reports");
        assert!(real.needs_human());
        assert_eq!(real.volumes.rows[0].state, VolumeState::Absent);
        assert!(real.pending.count == 0);
        assert!(root.path.is_dir());
    }

    #[test]
    fn a_kept_volume_is_never_pruned() {
        let harness = Harness::new("maintain-kept-volume");
        let mut record = harness.fixture.record.clone();
        record.durability = Durability::Kept;
        harness.fixture.register_record(&record);
        let root = harness.create_root(RootKind::AreaRoot);

        let report = harness.preview();
        let volume = &report.plan.volumes.rows[0];
        assert!(!volume.has_work());
        assert!(
            volume
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("kept"),
            "{:?}",
            volume.detail
        );
        assert!(root.path.is_dir());
    }

    #[test]
    fn a_volume_without_a_declared_durability_is_loud_and_prunes_nothing() {
        let harness = Harness::new("maintain-undeclared-volume");
        let mut record = harness.fixture.record.clone();
        record.durability = Durability::Undeclared;
        harness.fixture.register_record(&record);
        let root = harness.create_root(RootKind::AreaRoot);

        let report = harness.preview();
        let volume = &report.plan.volumes.rows[0];
        assert!(volume.loud);
        assert!(!volume.has_work());
        assert!(report.needs_human());
        assert!(root.path.is_dir());
    }

    #[test]
    fn a_revoked_claim_stops_only_the_claimed_unit() {
        let harness = Harness::new("maintain-revoked-claim");
        // One native scratch run and one unmarked unit covered by a claim: both
        // are candidates in the preview.
        let native = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&native, "owner");
        let claimed = harness.scratch_folder().join("alpha").join("old-run");
        std::fs::create_dir_all(&claimed).expect("the unmarked unit");
        std::fs::write(claimed.join("payload"), b"bytes").expect("the payload");
        let claim = ScratchClaim {
            scope: ClaimScope::Project {
                name: "alpha".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 1,
            families_excluded: Vec::new(),
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        scratch::declare_claim(&harness.fixture.state, &harness.fixture.uuid, &claim, false)
            .expect("the claim is declared");
        let preview = harness.preview();
        let scratch_step = preview.plan.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("a scratch stage");
        assert_eq!(scratch_step.plan.candidates.count, 2);

        // The claim is withdrawn inside the run's own window, at the claimed unit's
        // re-verification: the deletion's own authorization re-reads the record and
        // keeps that unit, while the native run the same plan named still goes. A
        // withdrawal *between* the preview and the real run is a registry revision
        // change instead, and the volume guard then keeps every planned unit (its
        // own test covers that).
        let state = harness.fixture.state.clone();
        let uuid = harness.fixture.uuid.clone();
        let withdrew = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = withdrew.clone();
        let faults = Faults::new();
        faults.observing(Box::new(move |name: &str| {
            if flag.get() || name != "old-run" {
                return;
            }
            flag.set(true);
            scratch::declare_claim(&state, &uuid, &claim, true).expect("the claim is withdrawn");
        }));
        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports");
        assert!(withdrew.get(), "the claim was withdrawn inside the window");
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("a scratch stage");
        assert_eq!(stage.deleted.count, 1, "{stage:?}");
        assert_eq!(stage.skipped.count, 1);
        assert_eq!(stage.skipped.rows[0].name, "old-run");
        // The withdrawal is a registry write, so the deletion's own per-unit
        // authorization names the changed record: the unit is kept and only this
        // unit is affected, which is the design's per-unit rule.
        assert!(
            stage.skipped.rows[0]
                .reason
                .contains("record changed since the plan"),
            "{}",
            stage.skipped.rows[0].reason
        );
        assert!(claimed.is_dir(), "the claimed unit is kept");
        assert!(!native.exists(), "the unaffected planned run still went");
        assert_eq!(report.pending.count, 0);
    }

    #[test]
    fn a_scratch_unit_that_became_marked_stops_only_that_unit() {
        let harness = Harness::new("maintain-became-marked");
        let native = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&native, "owner");
        let claimed = harness.scratch_folder().join("alpha").join("old-run");
        std::fs::create_dir_all(&claimed).expect("the unmarked unit");
        std::fs::write(claimed.join("payload"), b"bytes").expect("the payload");
        let claim = ScratchClaim {
            scope: ClaimScope::Project {
                name: "alpha".to_owned(),
            },
            unit: ClaimUnit::Child,
            unmarked_idle_seconds: 1,
            families_excluded: Vec::new(),
            declared_by: "operator".to_owned(),
            declared_ms: state::now_ms(),
        };
        scratch::declare_claim(&harness.fixture.state, &harness.fixture.uuid, &claim, false)
            .expect("the claim is declared");
        let preview = harness.preview();
        assert_eq!(
            preview.plan.volumes.rows[0]
                .scratch
                .as_ref()
                .expect("a scratch stage")
                .plan
                .candidates
                .count,
            2
        );

        // The unit gains a marker between the plan and the effect: a claim never
        // covers a marked unit, so this one is kept and the other still goes.
        let marker = claimed.join(area::ROOT_MARKER_FILE);
        std::fs::write(&marker, b"{\"schema\":\"barge.root/1\"}\n").expect("the marker");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600))
            .expect("the marker is private");
        let report = harness.real(&preview).expect("the run reports");
        let stage = report.volumes.rows[0]
            .scratch
            .as_ref()
            .expect("a scratch stage");
        assert_eq!(stage.deleted.count, 1, "{stage:?}");
        assert_eq!(stage.skipped.count, 1);
        assert_eq!(stage.skipped.rows[0].name, "old-run");
        assert!(claimed.is_dir(), "the newly marked unit is kept");
        assert!(!native.exists(), "the unaffected planned run still went");
    }

    #[test]
    fn maintain_reads_no_oxide_receipt_and_deletes_no_oxide_root() {
        let harness = Harness::new("maintain-oxide-untouched");
        // An Oxide outputs root and a receipt fixture, both outside every area
        // and every Barge folder.
        let output_root = harness.fixture.root.join("oxide-outputs").join("main");
        std::fs::create_dir_all(&output_root).expect("the oxide output root");
        std::fs::write(output_root.join("artifact"), b"bytes").expect("the artifact");
        let receipt = harness
            .fixture
            .state
            .root()
            .join("oxide-receipts")
            .join("prune.json");
        std::fs::create_dir_all(receipt.parent().expect("the receipt parent"))
            .expect("the receipts directory");
        std::fs::write(&receipt, b"{\"version\":1}\n").expect("the receipt");
        let before_receipt = std::fs::read(&receipt).expect("the receipt bytes");
        let before_entries = std::fs::read_dir(&output_root)
            .expect("the output root listing")
            .count();

        let idle = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let report = harness.real(&preview).expect("the run reports");
        assert!(report.volumes.rows[0].areas[0].deleted.count == 1);
        assert!(!idle.path.exists());
        assert_eq!(
            std::fs::read(&receipt).expect("the receipt is unchanged"),
            before_receipt
        );
        assert_eq!(
            std::fs::read_dir(&output_root)
                .expect("the output root listing")
                .count(),
            before_entries
        );
        assert!(output_root.is_dir());
    }

    #[test]
    fn an_error_inside_a_real_run_is_reported_as_a_stop_never_as_a_refusal() {
        let harness = Harness::new("maintain-mid-run-stop");
        let first = harness.create_root(RootKind::AreaRoot);
        let second = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        assert_eq!(harness.area_plan(&preview).candidates.count, 2);

        // One unit's own entry read fails inside its deletion window. Work may
        // already have happened, so the run must report the stop and need a
        // human rather than refuse with the exit code that says "nothing was
        // changed".
        let mut faults = Faults::new();
        faults.failing_facts_at(&second.root_id, 1);
        let report = harness
            .real_with(&faults, harness.aged(), &preview)
            .expect("the run reports instead of refusing");
        let stopped = report.stopped.as_deref().expect("the stop is reported");
        assert!(stopped.contains("stopped the run"), "{stopped}");
        assert!(report.needs_human());
        let area_stage = &report.volumes.rows[0].areas[0];
        assert_eq!(
            area_stage.deleted.count + area_stage.skipped.count,
            2,
            "every planned unit is accounted for"
        );
        assert!(
            area_stage
                .skipped
                .rows
                .iter()
                .any(|row| row.name == second.root_id
                    && row.reason.contains("the run stopped on this unit's error")),
            "{:?}",
            area_stage.skipped.rows
        );
        let _ = (first, second);
    }

    #[test]
    fn a_declaration_without_a_prune_policy_is_not_a_maintain_step() {
        let harness = Harness::new("maintain-no-policy");
        let mut declaration = harness.declaration();
        declaration.prune = PrunePolicy::None;
        area::declare(&harness.fixture.state, &declaration, harness.now_ms)
            .expect("the area is redeclared");
        let root = harness.create_root(RootKind::AreaRoot);
        let report = harness.preview();
        assert!(report.plan.volumes.rows[0].areas.rows.is_empty());
        assert!(root.path.is_dir());
    }

    #[test]
    fn the_scratch_stage_runs_before_the_areas_of_every_volume() {
        let harness = Harness::new("maintain-order");
        let scratch_root = harness.scratch_root("alpha", "owner");
        harness.conclude_and_release(&scratch_root, "owner");
        let root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let volume = &preview.plan.volumes.rows[0];
        assert!(volume.scratch.is_some(), "the scratch stage is first");
        assert_eq!(volume.areas.count, 1);
        // The registry order is the fixed volume order, and it is the same in
        // the preview and the real run.
        let report = harness.real(&preview).expect("the run reports");
        assert_eq!(report.plan.volumes, preview.plan.volumes);
        assert!(!root.path.exists());
        assert!(!scratch_root.exists());
    }

    #[test]
    fn a_record_whose_volume_is_not_the_attached_one_refuses_rather_than_reading_as_absent() {
        // The fixture mounts one volume. A record naming another UUID is a
        // mismatch, never an absence: absence is the one case a report may skip.
        let harness = Harness::new("maintain-mismatched-volume");
        let mut record = harness.fixture.record.clone();
        record.volume_uuid =
            crate::os::VolumeUuid::parse(crate::testkit::OTHER_UUID).expect("a second UUID");
        harness.fixture.register_record(&record);
        let platform = harness.fixture.platform();
        let error = maintain(
            &harness.fixture.state,
            &platform,
            &harness.fixture.facts(),
            &harness.fixture.mounts(),
            &harness.fixture.observer(),
            &harness.request(true, None, None, harness.aged()),
        )
        .expect_err("a record whose volume is not the attached one refuses");
        assert!(matches!(
            error.kind(),
            ErrorKind::VolumeMismatch | ErrorKind::VolumeAbsent
        ));
    }

    #[test]
    fn the_plan_reader_accepts_this_head_s_plan_and_refuses_other_documents() {
        let harness = Harness::new("maintain-plan-reader");
        let _root = harness.create_root(RootKind::AreaRoot);
        let preview = harness.preview();
        let bytes = canonical(&preview);
        let plan = super::read_maintain_plan(&bytes).expect("the plan reads");
        assert_eq!(plan, preview.plan);

        // An unknown major is refused, naming what this reader knows.
        let mut document: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the plan parses");
        document["schema"] = serde_json::Value::String("barge.maintain.plan/2".to_owned());
        let other = serde_json::to_vec(&document).expect("the document encodes");
        let error = super::read_maintain_plan(&other).expect_err("another major refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("unknown major"));

        // A field this major does not define is refused rather than ignored.
        let mut document: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the plan parses");
        document["extra"] = serde_json::Value::Bool(true);
        let extra = serde_json::to_vec(&document).expect("the document encodes");
        assert_eq!(
            super::read_maintain_plan(&extra)
                .expect_err("an unknown field refuses")
                .kind(),
            ErrorKind::UnknownState
        );
    }
}
