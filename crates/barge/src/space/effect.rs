//! The space prune's effect half (stage 3 design, "Pruning under the reserve",
//! "Concurrent pruning while consumers work", "Bounded resources" and the
//! failure matrix; slice T5).
//!
//! The preview half — [`super::plan`] — decides which idle owned roots may yield
//! under the reserve. This half applies that plan: an operator's real run presents
//! the preview's exact canonical bytes and their SHA-256, and the pass re-verifies
//! every predicate at each candidate's effect step before the stage 1 custody,
//! intent and quarantine protocol deletes it. There is no second delete path: the
//! same [`crate::delete`] sequence runs, with the pass's volume lock supplied
//! instead of taken.
//!
//! The rules this module carries:
//!
//! - **The plan is the authorization.** A plan whose bytes do not match its
//!   digest, or that its own reader refuses, is refused with no effect, and so is
//!   a busy volume lock (`locked`). Nothing is recomputed and compared: one
//!   candidate's change keeps that candidate only.
//! - **One prune per volume.** The exclusive volume lock is held from the first
//!   authoritative measurement to the final decision, so two prunes on one volume
//!   cannot overlap, and each candidate's root lock is taken after it, one root at
//!   a time. A consumer's `hold` therefore waits for at most one deletion.
//! - **The policy is a versioned snapshot.** Before each effect the registry is
//!   re-read (bounded, lock-free) and its `generation` compared. A changed
//!   generation is not a refusal: the pass adopts the new policy, re-measures and
//!   re-evaluates the candidate, skipping it as `policy_changed` when it no longer
//!   qualifies. Only an unreadable or invalid registry, an undeclared durability
//!   and a durability that became `kept` stop the pass.
//! - **Nothing is re-used from the plan that the effect step cannot re-prove.**
//!   Each candidate's identity digest (device, inode and marker digest), its
//!   marker and root id, its durable `barge.root.identity/1` record, its pressure
//!   idleness, its area's `counts` participation, its pending intents, the same-uid
//!   descriptor scan and the volume are all re-read under the locks the deletion
//!   holds. A held root — including an older one the LRU order reaches first — is
//!   skipped and reported as `held`, never deferred silently.
//! - **Flush and reclaim.** One flush per batch (30 s bound), then re-measure. The
//!   pass stops when free space is strictly above `max(reserve, floor)`, when
//!   nothing eligible remains, when a batch that deleted something reclaimed
//!   nothing (`space_not_reclaimed`), or when a bound is reached
//!   (`budget_exhausted`). A failed flush, a failed post-flush reverify or an
//!   unreadable measurement is `unknown_state` at exit `3`, naming what was
//!   already deleted.
//! - **Bounds.** At most 128 roots in at most 4 batches of 32, a 120 s pass, a
//!   per-root deadline of `min(120 s, remaining - 5 s)` and no effect started
//!   inside the last 5 s. A bound that is reached reports what it did not reach,
//!   with counts.
//! - **Replay first.** A real run settles what an earlier one left pending before
//!   it deletes anything, and only after the volume re-verifies and each intent's
//!   recorded identity matches. An unsettled intent keeps its own object; other
//!   candidates continue. An unreadable journal stops the pass.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    read_plan, state_name, SpaceCandidateRow, SpaceMeasurement, SpacePrunePlan, SpacePruneReport,
};
use crate::area::{self, AreaDeclaration, AreaDocument, MarkerStateName, ReserveParticipation};
use crate::contract::{RowSet, INTENT_ROWS, ROOT_ROWS};
use crate::custody::PendingIntent;
use crate::delete::{self, DeleteBounds, DeleteReport};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::identity::{self, IdentityReason, IdentityVerdict};
use crate::os::{
    AbsenceVolume, DeviceId, DirOps, Flush, MountRoot, ProcessObserver, SpaceFacts, VerifiedVolume,
    VolumeUuid,
};
use crate::registry::{self, SpacePolicy, VolumeRecord};
use crate::scratch;
use crate::state::{self, Deadline, LockSet, StatePaths};
use crate::unit::{self, DeletionUnit, MarkerFamily, UnitGrant, UnitKind, UnitMarker};

/// One real prune pass's wall clock (design, "Bounded resources": the operator's
/// real run uses the guard-call bounds with a fixed 120 s clock and no option).
pub const SPACE_PRUNE_WALL: Duration = Duration::from_secs(120);

/// How many roots one pass may delete (the design's 4 batches of 32).
pub const SPACE_PRUNE_ROOTS: usize = 128;

/// How many roots one batch may delete.
pub const SPACE_PRUNE_BATCH_ROOTS: usize = 32;

/// How many batches one pass may run.
pub const SPACE_PRUNE_BATCHES: usize = 4;

/// How much of the pass is reserved for stopping: no effect starts inside it.
pub const SPACE_PRUNE_STOP_MARGIN: Duration = Duration::from_secs(5);

/// The longest one root's deletion may take inside a pass.
pub const SPACE_PRUNE_ROOT_DEADLINE: Duration = Duration::from_secs(120);

/// How long one batch's flush may take before the pass reports a reached bound.
pub const SPACE_FLUSH_DEADLINE: Duration = Duration::from_secs(30);

/// One pass's bounds. The design's are the constants above; a test shortens them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpacePruneBounds {
    /// The whole pass's wall clock.
    pub wall: Duration,
    /// The most roots one pass may delete.
    pub roots: usize,
    /// The most roots one batch may delete.
    pub batch_roots: usize,
    /// The most batches one pass may run.
    pub batches: usize,
    /// The window at the end in which no effect starts.
    pub stop_margin: Duration,
    /// The longest one root's deletion may take.
    pub root_deadline: Duration,
    /// The longest one batch's flush may take.
    pub flush_deadline: Duration,
}

impl SpacePruneBounds {
    /// The design's bounds.
    pub const fn design() -> Self {
        Self {
            wall: SPACE_PRUNE_WALL,
            roots: SPACE_PRUNE_ROOTS,
            batch_roots: SPACE_PRUNE_BATCH_ROOTS,
            batches: SPACE_PRUNE_BATCHES,
            stop_margin: SPACE_PRUNE_STOP_MARGIN,
            root_deadline: SPACE_PRUNE_ROOT_DEADLINE,
            flush_deadline: SPACE_FLUSH_DEADLINE,
        }
    }

    /// The moment no effect may start.
    fn effect_deadline(self, started: Instant) -> Instant {
        started + self.wall.saturating_sub(self.stop_margin)
    }
}

impl Default for SpacePruneBounds {
    fn default() -> Self {
        Self::design()
    }
}

/// What one caller asks a real space prune for.
#[derive(Debug, Clone, Copy)]
pub struct SpacePruneRunRequest<'a> {
    /// The registered volume the caller named.
    pub volume_uuid: &'a VolumeUuid,
    /// The SHA-256 of the preview's exact plan bytes.
    pub plan_digest: &'a str,
    /// The preview's exact plan bytes.
    pub plan_bytes: &'a [u8],
    /// The run's clock, so a test can place a release in the past.
    pub now_ms: u64,
    /// How long a lock may wait for a live owner.
    pub lock_wait: Duration,
    /// The deletion's own bounds, which the pass narrows per root.
    pub delete_bounds: DeleteBounds,
    /// The pass's bounds.
    pub bounds: SpacePruneBounds,
}

/// One root this pass deleted, in delete order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacePrunedRow {
    /// The root the deletion acted on.
    pub root_id: String,
    /// The area it belonged to, as `namespace/area`.
    pub area: String,
    /// Its directory name inside its folder, which need not be its root id.
    pub name: String,
    /// The path the plan named.
    pub path: PathBuf,
    /// The bytes the plan counted for it, which are not the bytes reclaimed.
    pub bytes: u64,
    /// Its recorded release, which ordered it.
    pub last_release_ms: u64,
    /// The intent that carried the deletion.
    pub intent_id: String,
    /// How many entries the deletion removed, the marker included.
    pub entries_removed: usize,
    /// How many directories it removed.
    pub directories_removed: usize,
    /// Whether its marker was unlinked last.
    pub marker_removed: bool,
}

/// One planned root this pass kept, with the reason its effect step found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacePruneHeldRow {
    /// The root id the plan named.
    pub root_id: String,
    /// The area it belonged to, as `namespace/area`.
    pub area: String,
    /// Its directory name inside its folder.
    pub name: String,
    /// Why it was kept: `held`, `policy_changed`, `identity_changed`,
    /// `not_idle`, `pending_intent`, `locked` or `refused`.
    pub reason: String,
    /// What the re-verification actually read, for a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One planned root a bound or a stop left unreached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceUnreachedRow {
    /// The root id the plan named.
    pub root_id: String,
    /// The area it belonged to, as `namespace/area`.
    pub area: String,
    /// Its directory name inside its folder.
    pub name: String,
    /// The bytes the plan counted for it, so the unreclaimed capacity is a number
    /// and not a claim that nothing was there.
    pub bytes: u64,
    /// Why the pass did not reach it.
    pub reason: String,
}

/// One pass's flush record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceFlushRow {
    /// How many flushes the pass asked for: one after scoped replay work,
    /// followed by one per batch.
    pub attempts: usize,
    /// How many failed. A failure stops the pass, so this is `0` or `1`.
    pub failures: usize,
    /// How long the last flush took in milliseconds.
    pub last_ms: u64,
    /// The flush's own bound in milliseconds.
    pub bound_ms: u64,
}

/// What a real prune pass did, and what it did not reach.
///
/// It is `null` in a preview, which changes nothing. Every count here is the
/// pass's own accounting: `reclaimed_bytes` is the measured difference between
/// the pass's first authoritative measurement and its last, never a sum of
/// recorded root sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacePruneEffect {
    /// When the pass started, after the plan was read.
    pub started_ms: u64,
    /// When the pass finished, which is when its last measurement completed.
    pub finished_ms: u64,
    /// The pass's first authoritative measurement, taken under the volume lock.
    pub before: SpaceMeasurement,
    /// The pass's last measurement: the one it decided on.
    pub after: SpaceMeasurement,
    /// The policy in force when the pass stopped, which may be newer than the
    /// plan's when a generation change was adopted.
    pub policy: SpacePolicy,
    /// The registry generation in force when the pass stopped.
    pub policy_generation: u64,
    /// `max(reserve_bytes, floor_bytes)` of the policy in force.
    pub target_bytes: u64,
    /// How many batches ran.
    pub batches: usize,
    /// How many flush-and-measure rounds ran.
    pub rounds: usize,
    /// The roots the pass deleted, in delete order.
    pub pruned: RowSet<SpacePrunedRow>,
    /// The planned roots the pass kept, with the reason each effect step found.
    pub held: RowSet<SpacePruneHeldRow>,
    /// The allocated bytes the deleted roots' plans counted. It is not the
    /// reclaimed figure.
    pub deleted_bytes: u64,
    /// The measured difference across the pass.
    pub reclaimed_bytes: u64,
    /// Why the pass stopped: `target_reached`, `nothing_eligible`,
    /// `space_not_reclaimed` or `budget_exhausted`.
    pub stop: String,
    /// The pass's flush accounting.
    pub flush: SpaceFlushRow,
    /// Whether the guard's live plan was truncated (internal to the guard).
    #[serde(skip)]
    pub(super) live_plan_truncated: bool,
    /// Root rows the bounded live planner did not reach.
    #[serde(skip)]
    pub(super) live_plan_unreached: usize,
    /// What the run replayed and flushed before its first measurement, when it
    /// had any scoped intents to examine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<SpaceReplayRow>,
    /// The planned roots the pass never attempted, with counts.
    pub not_reached: RowSet<SpaceUnreachedRow>,
}

impl SpacePruneEffect {
    /// Whether the operator must read this pass before relying on it: it ended on
    /// a bound, or it deleted data for no measured reclaim. Neither is reported as
    /// a pass that met its policy.
    pub fn needs_human(&self) -> bool {
        matches!(
            self.stop.as_str(),
            "budget_exhausted" | "space_not_reclaimed"
        )
    }
}

/// What one run's replay of earlier pending intents found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceReplayRow {
    /// How many intents the journal held.
    pub examined: usize,
    /// How many the both-absent proof settled.
    pub settled: usize,
    /// How many were rolled back because the unit was still in place.
    pub rolled_back: usize,
    /// How many resumed a deletion and completed it.
    pub resumed: usize,
    /// How many are still pending, keeping only their covered objects.
    pub pending: usize,
}

impl From<&delete::ReplayReport> for SpaceReplayRow {
    fn from(report: &delete::ReplayReport) -> Self {
        Self {
            examined: report.examined,
            settled: report.settled,
            rolled_back: report.rolled_back,
            resumed: report.resumed,
            pending: report.pending.len(),
        }
    }
}

/// Why one candidate's effect step kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Skip {
    reason: &'static str,
    detail: String,
}

impl Skip {
    fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    /// The error the eligibility check returns so the deletion stops before its
    /// first effect. The caller reads this `Skip` back from its own slot, so the
    /// kind is only a carrier and never the answer.
    fn error(&self) -> Error {
        Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the space prune kept this root ({}): {}",
                self.reason, self.detail
            ),
        )
    }
}

/// How one candidate's effect ended.
enum Outcome {
    /// The deletion completed and its intent settled.
    Deleted(Box<SpacePrunedRow>),
    /// The candidate was kept, with the reason.
    Skipped(Skip),
    /// A failure that is not one candidate's refusal stops the pass.
    Stop(Error),
}

/// The folder and declaration one planned row is acted on with.
struct Target {
    declaration: AreaDeclaration,
    document: Option<AreaDocument>,
    folder: String,
}

/// A target resolution's three answers: act, keep this row, or stop the pass.
enum ResolvedTarget {
    Ready(Box<Target>),
    Skipped(Skip),
    Stop(Error),
}

/// Delete the plan's candidates until the policy is satisfied or a bound stops the
/// pass, re-verifying each candidate at its effect step.
///
/// The plan is the authorization: its exact bytes and their digest are presented,
/// its own reader must accept them, and every candidate is re-proved under the
/// volume and root locks the deletion holds. A candidate that changed is kept and
/// reported; a registry that cannot be read, or a durability that no longer
/// permits pruning, stops the pass with `unknown_state`/`refused_boundary` (exit
/// `3`) naming what was already deleted.
#[allow(clippy::too_many_arguments)]
pub fn prune(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    flush: &dyn Flush,
    request: &SpacePruneRunRequest<'_>,
) -> Result<SpacePruneReport> {
    // The plan is the authorization, so its bytes and its digest are checked before
    // anything else: a mismatched plan is a refusal that changes nothing.
    if !unit::is_sha256(request.plan_digest)
        || sha256_hex(request.plan_bytes) != request.plan_digest
    {
        return Err(Error::new(
            ErrorKind::Usage,
            "the plan file's bytes do not match --plan-sha256; nothing was changed",
        )
        .with_detail("plan_digest", request.plan_digest.into()));
    }
    let plan = read_plan(request.plan_bytes).map_err(|error| {
        Error::new(
            error.kind(),
            format!(
                "the plan was refused, so nothing was changed: {}",
                error.message()
            ),
        )
    })?;
    prune_live(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        space,
        flush,
        request,
        Some(plan),
    )
    .map(|(report, _lock, _replay)| report)
}

/// Apply an in-memory plan through the same custody and effect path.
#[allow(clippy::too_many_arguments)]
pub(super) fn prune_live(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    flush: &dyn Flush,
    request: &SpacePruneRunRequest<'_>,
    plan: Option<SpacePrunePlan>,
) -> Result<(
    SpacePruneReport,
    crate::state::StateLock,
    delete::ReplayReport,
)> {
    if let Some(plan) = &plan {
        if &plan.volume_uuid != request.volume_uuid {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "the plan names volume {} but --volume names {}; nothing was changed",
                    plan.volume_uuid, request.volume_uuid
                ),
            )
            .with_detail("plan_volume_uuid", plan.volume_uuid.as_str().into())
            .with_detail("volume_uuid", request.volume_uuid.as_str().into()));
        }
    }
    let started = Instant::now();
    let deadline = started + request.bounds.wall;
    let effect_deadline = request.bounds.effect_deadline(started);

    // Pre-flight, effect-free: the volume must be registered, its durability must
    // permit pruning and its declared policy must exist. A refusal here changes
    // nothing, so it is an ordinary refusal document.
    let record = super::guard::registered_volume_with_budget(
        paths,
        request.volume_uuid,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let durability = record.declared_durability()?;
    if !durability.permits_pruning() {
        // The failure matrix's durability row: a volume that may not be pruned is
        // unknown state at exit 3, not a no-effect refusal at exit 2. The operator
        // asked for a real prune of data the volume no longer says is expendable, so
        // the answer names the durability and needs their decision.
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "volume {} is declared `{}`, which never permits pruning, so nothing was changed",
                record.volume_uuid,
                super::durability_name(durability)
            ),
        )
        .with_detail("volume_uuid", record.volume_uuid.as_str().into())
        .with_detail("durability", super::durability_name(durability).into()));
    }
    // A volume with no declared policy refuses the run, exactly as it refuses a
    // plan: nothing about a missing threshold is defaulted.
    record.declared_space()?;
    let mount = record.mount_hint.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "registered volume has no mount hint, so this run cannot verify the volume it would prune",
        )
    })?;
    let verified = crate::os::verify_volume(facts, mounts, &mount, &record.expected_volume())?;

    // One prune per volume: the volume's exclusive lock is held from this first
    // authoritative measurement to the final decision.
    paths.initialize()?;
    let mut locks = LockSet::with_deadline(
        paths.clone(),
        super::lock_deadline(effect_deadline, request.lock_wait),
    );
    let _volume_lock = locks.lock_volume(request.volume_uuid).map_err(|error| {
        if error.kind() != ErrorKind::Locked {
            return error;
        }
        Error::new(
            ErrorKind::Locked,
            format!(
                "volume {}'s lock could not be taken for this prune: {}; nothing was changed",
                request.volume_uuid,
                error.message()
            ),
        )
        .with_detail("volume_uuid", request.volume_uuid.as_str().into())
        .with_detail(
            "lock",
            paths
                .volume_lock_path(request.volume_uuid)
                .display()
                .to_string()
                .into(),
        )
    })?;
    // Registration takes this volume lock too. The preflight record may have
    // become stale while we waited, so no replay may use its pruning authority.
    let current = super::guard::registered_volume_with_budget(
        paths,
        request.volume_uuid,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let durability = current.declared_durability()?;
    if !durability.permits_pruning() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the volume no longer permits pruning; replay and deletion were not started",
        )
        .with_detail("volume_uuid", request.volume_uuid.as_str().into())
        .with_detail("durability", super::durability_name(durability).into()));
    }
    // Replay next, under that lock and **only** for this volume: a real run settles
    // what an earlier one left pending before it deletes anything, from the
    // recorded identities only, after the volume re-verifies. Another volume's
    // pending intent is a different object's business and never blocks, delays or
    // is acted on here. An intent of this volume that stays pending stops the pass
    // only for its own object; unrelated candidates can still proceed.
    let replay = delete::replay_pending_on_volume(
        paths,
        dirs,
        facts,
        observer,
        request.volume_uuid,
        &_volume_lock,
        DeleteBounds {
            deadline: Some(deadline),
            ..request.delete_bounds
        },
        Some(deadline),
    )?;
    // An intent this run could not settle is **that object's** business (ruling
    // `698d9ba3`): the pass continues, and every candidate the intent covers is
    // skipped and reported as `pending_intent`. It never stops a pass over
    // unrelated paths, and the report carries the replay's own counts so the
    // operator can see what stayed pending.
    let mut flush_row = SpaceFlushRow {
        attempts: 0,
        failures: 0,
        last_ms: 0,
        bound_ms: u64::try_from(request.bounds.flush_deadline.as_millis()).unwrap_or(u64::MAX),
    };
    // A pending replay can have removed entries without completing the unit.
    // Its completion counters therefore cannot prove that nothing was deleted.
    // Conservatively flush any scoped replay work before the first measurement
    // or a new deletion. A replay-free pass needs no preliminary flush.
    if replay.examined > 0 {
        flush_verified_volume(
            flush,
            facts,
            &verified,
            deadline,
            request.bounds.flush_deadline,
            &mut flush_row,
        )
        .map_err(|error| {
            with_replay_detail(error, &replay)
                .with_detail("replay", serde_json::json!(&replay))
                .with_detail("flush", serde_json::json!(&flush_row))
        })?;
    }
    let live = plan.is_none();
    let before = space.measure_space(&verified).map_err(|error| {
        // An absence observed after replay is not a no-effect refusal: replay
        // may already have deleted. Preserve the facts but require exit 3.
        let failure = if replay.examined > 0 || (live && error.kind() != ErrorKind::VolumeAbsent) {
            let mut failure = Error::new(
                ErrorKind::UnknownState,
                format!("space is unknown after replay: {error}"),
            );
            for (key, value) in error.detail() {
                failure = failure.with_detail(key, value.clone());
            }
            failure
        } else {
            error
        };
        with_replay_detail(failure, &replay)
            .with_detail("replay", serde_json::json!(&replay))
            .with_detail("flush", serde_json::json!(&flush_row))
    })?;
    let (plan, plan_sha256, live_plan_truncated, live_plan_unreached) = if let Some(plan) = plan {
        (plan, request.plan_digest.to_owned(), false, 0)
    } else {
        let planned = super::plan_locked(
            paths,
            dirs,
            facts,
            mounts,
            observer,
            &super::SpacePruneRequest {
                volume_uuid: request.volume_uuid,
                now_ms: request.now_ms,
                wall: deadline
                    .saturating_duration_since(Instant::now())
                    .min(super::SPACE_PLAN_WALL),
                lock_wait: request.lock_wait,
            },
            before.clone(),
            &_volume_lock,
        )
        .map_err(|error| {
            let mut unknown = Error::new(
                ErrorKind::UnknownState,
                format!("live planning failed after replay: {error}"),
            );
            for (key, value) in error.detail() {
                unknown = unknown.with_detail(key, value.clone());
            }
            with_replay_detail(unknown, &replay)
                .with_detail("replay", serde_json::json!(&replay))
                .with_detail("pruned_root_ids", serde_json::json!([]))
        })?;
        (
            planned.plan,
            planned.plan_sha256,
            planned.truncated,
            planned.unreached,
        )
    };
    // The plan's own snapshot is what authorizes the run. A policy published between
    // the plan and this pass is the first generation change an effect step
    // re-evaluates, never a silent upgrade of what the plan meant.
    let planned_policy = plan.policy.clone();
    let planned_generation = plan.policy_generation;

    let mut pass = Pass {
        paths,
        dirs,
        facts,
        mounts,
        observer,
        space,
        flush,
        volume_lock: Some(_volume_lock),
        verified,
        record,
        mount,
        plan,
        plan_sha256,
        live,
        live_plan_truncated,
        live_plan_unreached,
        dirty: false,
        measurements: if live { 2 } else { 0 },
        now_ms: request.now_ms,
        effect_deadline,
        wall_deadline: deadline,
        bounds: request.bounds,
        delete_bounds: request.delete_bounds,
        policy: planned_policy,
        policy_generation: planned_generation,
        before: before.clone(),
        current: before,
        pruned: Vec::new(),
        held: Vec::new(),
        deleted_bytes: 0,
        unreached: Vec::new(),
        flush_row,
        replay,
        stop: None,
        batches: 0,
        rounds: 0,
        started_ms: request.now_ms,
    };
    pass.run()?;
    let lock = pass
        .volume_lock
        .take()
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "the volume lock was lost"))?;
    let replay = pass.replay.clone();
    Ok((pass.into_report(started), lock, replay))
}

fn with_replay_detail(error: Error, replay: &delete::ReplayReport) -> Error {
    let mut completed = replay.resumed_root_ids.clone();
    completed.extend(replay.completed_by_settlement_root_ids.clone());
    error
        .with_detail("replayed_root_ids", serde_json::json!(completed))
        .with_detail(
            "replay_partial_root_ids",
            serde_json::json!(replay.partial_root_ids),
        )
        .with_detail(
            "replay_settled_root_ids",
            serde_json::json!(replay.settled_root_ids),
        )
}

/// The shared replay/batch flush boundary. A failed flush never authorizes a
/// measurement, and uses exactly the same deadline, reap and identity rules.
fn flush_verified_volume(
    flush: &dyn Flush,
    facts: &dyn AbsenceVolume,
    volume: &VerifiedVolume,
    wall_deadline: Instant,
    bound: Duration,
    row: &mut SpaceFlushRow,
) -> Result<()> {
    row.attempts = row.attempts.saturating_add(1);
    let began = Instant::now();
    // Reserve the helper's bounded reap window inside the pass wall clock.
    let deadline = (began + bound).min(
        wall_deadline
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(began),
    );
    let flushed = flush.flush_volume(volume, deadline);
    let elapsed = began.elapsed();
    row.last_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    if let Err(error) = flushed {
        row.failures = row.failures.saturating_add(1);
        let mut failure = Error::new(
            ErrorKind::UnknownState,
            format!("the volume flush failed; post-prune space is unknown: {error}"),
        );
        for (key, value) in error.detail() {
            failure = failure.with_detail(key, value.clone());
        }
        return Err(failure.with_detail("flush_failed", true.into()));
    }
    // A drive pulled during the flush is a reverify failure, not a measurement
    // of zero: the post-prune state is unknown and the pass says so.
    volume.reverify(facts).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the volume could not be re-verified after the flush, so the post-prune measurement is unknown: {error}"
                ),
            )
        })?;
    Ok(())
}

/// One real pass's state, so the loop, the effect step and the report all read the
/// same facts.
struct Pass<'a> {
    paths: &'a StatePaths,
    dirs: &'a dyn DirOps,
    facts: &'a dyn AbsenceVolume,
    mounts: &'a dyn MountRoot,
    observer: &'a dyn ProcessObserver,
    space: &'a dyn SpaceFacts,
    flush: &'a dyn Flush,
    /// The volume's exclusive lock token, held from the first authoritative
    /// measurement to the final decision. Each candidate's deletion takes its own
    /// root lock in a lock set of its own, so only one root lock is ever held.
    volume_lock: Option<crate::state::StateLock>,
    verified: VerifiedVolume,
    record: VolumeRecord,
    mount: PathBuf,
    plan: SpacePrunePlan,
    plan_sha256: String,
    live: bool,
    live_plan_truncated: bool,
    live_plan_unreached: usize,
    dirty: bool,
    measurements: usize,
    now_ms: u64,
    /// The moment no effect may start.
    effect_deadline: Instant,
    wall_deadline: Instant,
    bounds: SpacePruneBounds,
    delete_bounds: DeleteBounds,
    policy: SpacePolicy,
    policy_generation: u64,
    /// The first authoritative measurement.
    before: SpaceMeasurement,
    /// The last measurement, which is the one the stop is decided on.
    current: SpaceMeasurement,
    pruned: Vec<SpacePrunedRow>,
    held: Vec<SpacePruneHeldRow>,
    deleted_bytes: u64,
    unreached: Vec<SpaceUnreachedRow>,
    flush_row: SpaceFlushRow,
    replay: delete::ReplayReport,
    stop: Option<&'static str>,
    batches: usize,
    rounds: usize,
    started_ms: u64,
}

impl Pass<'_> {
    /// The prune target of the policy in force: `max(reserve, floor)`.
    fn target(&self) -> u64 {
        self.policy.reserve_bytes.max(self.policy.floor_bytes)
    }

    /// The bytes the pass measured back across its own window.
    fn reclaimed(&self) -> u64 {
        self.current
            .free_bytes
            .saturating_sub(self.before.free_bytes)
    }

    /// Run the bounded pass: batches of the plan's candidates, one flush and one
    /// measurement per batch, stopping on the policy or on a bound.
    fn run(&mut self) -> Result<()> {
        let mut candidates = self.plan.candidates.rows.clone();
        let mut cursor = 0usize;
        // The policy is re-read **before** the first decision, not trusted from the
        // plan: an operator's real run never acts on a stale threshold and never
        // stops on one. A policy published between the plan and this pass is
        // adopted here, with a fresh measurement, exactly as it is before every
        // later effect; only then does the pass decide whether there is anything to
        // reclaim.
        match self.refresh_policy() {
            Ok(true) => {}
            Ok(false) => {
                if self.stop.is_none() {
                    self.stop = Some("target_reached");
                }
            }
            Err(error) => return Err(self.annotate(error)),
        }
        if self.stop.is_none() && self.current.free_bytes > self.target() {
            // The volume already satisfies the policy in force, so there is nothing
            // to reclaim: an operator's real run never deletes data it was not
            // asked to free.
            self.stop = Some("target_reached");
        }
        while self.stop.is_none() {
            if (self.live && self.measurements >= 4)
                || self.pruned.len() >= self.bounds.roots
                || self.batches >= self.bounds.batches
                || Instant::now() >= self.effect_deadline
            {
                self.stop = Some("budget_exhausted");
                break;
            }
            if cursor >= candidates.len() {
                self.stop = Some(if self.live_plan_truncated {
                    "budget_exhausted"
                } else {
                    "nothing_eligible"
                });
                break;
            }
            let take = self
                .bounds
                .batch_roots
                .min(self.bounds.roots.saturating_sub(self.pruned.len()));
            let end = cursor.saturating_add(take).min(candidates.len());
            let batch = candidates[cursor..end].to_vec();
            self.batches = self.batches.saturating_add(1);
            let batch_start = self.current.free_bytes;
            let mut batch_deleted = 0usize;
            let mut cut_short = false;
            for candidate in &batch {
                if self.stop.is_some() || Instant::now() >= self.effect_deadline {
                    // The bound stops this batch where it is; the rows it did not
                    // attempt are reported as unreached below.
                    self.stop = Some("budget_exhausted");
                    cut_short = true;
                    break;
                }
                if self.pruned.len() >= self.bounds.roots {
                    self.stop = Some("budget_exhausted");
                    cut_short = true;
                    break;
                }
                match self.effect(candidate) {
                    Outcome::Deleted(row) => {
                        self.dirty = true;
                        self.deleted_bytes = self.deleted_bytes.saturating_add(row.bytes);
                        self.pruned.push(*row);
                        batch_deleted = batch_deleted.saturating_add(1);
                    }
                    Outcome::Skipped(skip) => self.held.push(SpacePruneHeldRow {
                        root_id: candidate.root_id.clone(),
                        area: candidate.area.clone(),
                        name: candidate.name.clone(),
                        reason: skip.reason.to_owned(),
                        detail: Some(skip.detail),
                    }),
                    Outcome::Stop(error) => return Err(self.annotate(error)),
                }
                cursor = cursor.saturating_add(1);
            }
            // One flush per batch, then re-measure: the reclaim check is a measured
            // fact, never a sum of the sizes the plan recorded. A batch a bound cut
            // short is still flushed when it deleted something, so no deletion is
            // ever reported from an unflushed measurement; a batch that deleted
            // nothing has no reclaim to measure.
            if (!self.live && !cut_short) || batch_deleted > 0 {
                if let Err(error) = self.flush_and_measure() {
                    return Err(self.annotate(error));
                }
                self.dirty = false;
                self.rounds = self.rounds.saturating_add(1);
            }
            if self.stop.is_some() {
                // A bound ended the pass; a flush that outlived its own bound is a
                // reached bound too, and the measurement beside it is still the one
                // the report carries.
                break;
            }
            if self.current.free_bytes > self.target() {
                self.stop = Some("target_reached");
            } else if batch_deleted > 0 && self.current.free_bytes <= batch_start {
                // Something was deleted and the volume gave nothing back. The cause
                // — a foreign writer's unlinked-but-open file, purgeable space, a
                // filesystem that has not committed — is undecidable, so the pass
                // stops rather than deleting more data for no effect.
                self.stop = Some("space_not_reclaimed");
            } else if self.live && batch_deleted > 0 {
                self.now_ms = self.now_ms.max(state::now_ms());
                let planned = super::plan_locked(
                    self.paths,
                    self.dirs,
                    self.facts,
                    self.mounts,
                    self.observer,
                    &super::SpacePruneRequest {
                        volume_uuid: &self.record.volume_uuid,
                        now_ms: self.now_ms,
                        wall: self
                            .effect_deadline
                            .saturating_duration_since(Instant::now())
                            .min(super::SPACE_PLAN_WALL),
                        lock_wait: self.bounds.wall.min(std::time::Duration::from_secs(5)),
                    },
                    self.current.clone(),
                    self.volume_lock.as_ref().ok_or_else(|| {
                        Error::new(ErrorKind::UnknownState, "the volume lock was lost")
                    })?,
                )
                .map_err(|error| self.annotate(error))?;
                self.live_plan_truncated |= planned.truncated;
                self.live_plan_unreached = self.live_plan_unreached.max(planned.unreached);
                candidates = planned.plan.candidates.rows;
                cursor = 0;
                if candidates.is_empty() {
                    self.stop = Some(if self.live_plan_truncated {
                        "budget_exhausted"
                    } else {
                        "nothing_eligible"
                    });
                }
            } else if cursor >= candidates.len() || (self.live && batch_deleted == 0) {
                self.stop = Some(if self.live_plan_truncated {
                    "budget_exhausted"
                } else {
                    "nothing_eligible"
                });
            }
        }
        // The pass accounts for every planned root: one it never attempted is
        // reported with its bytes and the reason, never dropped as if the plan had
        // been shorter.
        let stop = self.stop.unwrap_or("nothing_eligible");
        let attempted: Vec<&str> = self
            .pruned
            .iter()
            .map(|row| row.root_id.as_str())
            .chain(self.held.iter().map(|row| row.root_id.as_str()))
            .collect();
        for row in self.plan.candidates.rows.iter().chain(candidates.iter()) {
            if attempted.contains(&row.root_id.as_str())
                || self
                    .unreached
                    .iter()
                    .any(|other| other.root_id == row.root_id)
            {
                continue;
            }
            self.unreached.push(SpaceUnreachedRow {
                root_id: row.root_id.clone(),
                area: row.area.clone(),
                name: row.name.clone(),
                bytes: row.bytes,
                reason: stop.to_owned(),
            });
        }
        Ok(())
    }

    /// The policy in force, re-read from the registry before every effect.
    ///
    /// A changed generation is not a refusal: the pass adopts the new policy and
    /// continues from a fresh measurement, because the new thresholds may be
    /// different. An unreadable or invalid registry, an undeclared durability and a
    /// durability that became `kept` are the only things that stop the pass here.
    fn refresh_policy(&mut self) -> Result<bool> {
        let registry = registry::read_with_deadline(self.paths, Deadline::after(self.wall_deadline.saturating_duration_since(Instant::now()).min(state::READ_DEADLINE))).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the registry could not be read again before an effect, so this pass stopped: {error}"
                ),
            )
            .with_detail(
                "registry",
                self.paths.registry_path().display().to_string().into(),
            )
        })?;
        let Some(record) = registry.volume(&self.record.volume_uuid).cloned() else {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "volume {} left the registry before an effect, so this pass stopped",
                    self.record.volume_uuid
                ),
            )
            .with_detail(
                "registry",
                self.paths.registry_path().display().to_string().into(),
            ));
        };
        let durability = record.declared_durability()?;
        if !durability.permits_pruning() {
            // A pass may already have deleted roots, so a durability that stopped
            // permitting pruning is unknown state at exit 3, never a no-effect
            // refusal at exit 2: the operator must read what the pass did.
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "volume {} is now declared `{}`, which never permits pruning, so this pass stopped",
                    record.volume_uuid,
                    super::durability_name(durability)
                ),
            )
            .with_detail("volume_uuid", record.volume_uuid.as_str().into())
            .with_detail("durability", super::durability_name(durability).into()));
        }
        let policy = record.declared_space()?.clone();
        let changed = record.generation != self.policy_generation;
        self.record = record;
        if changed || policy != self.policy {
            // Re-evaluate against the new policy, from a fresh measurement: the
            // thresholds the pass now applies are the new ones, and a candidate
            // the new policy no longer needs is kept.
            self.policy = policy;
            self.policy_generation = self.record.generation;
            if self.live && self.measurements >= 3 {
                self.stop = Some("budget_exhausted");
                if !self.dirty && self.measurements < 4 {
                    self.measure()?;
                }
                return Ok(false);
            }
            self.measure()?;
            if self.current.free_bytes > self.target() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Measure the volume without flushing and adopt the fresh fact.
    fn measure(&mut self) -> Result<()> {
        self.measurements = self.measurements.saturating_add(1);
        let measurement = self.space.measure_space(&self.verified).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!("the volume could not be measured again, so this pass stopped: {error}"),
            )
        })?;
        self.current = measurement;
        Ok(())
    }

    /// Flush the batch before the reclaim measurement, using the same boundary
    /// that protects the first measurement after replay.
    fn flush_and_measure(&mut self) -> Result<()> {
        flush_verified_volume(
            self.flush,
            self.facts,
            &self.verified,
            self.wall_deadline,
            self.bounds.flush_deadline,
            &mut self.flush_row,
        )?;
        self.measure()
    }

    /// One candidate's effect: re-read the policy, re-resolve its folder, build the
    /// stage 1 unit and delete it through the same custody, intent and quarantine
    /// path every other Barge deletion uses.
    fn effect(&mut self, candidate: &SpaceCandidateRow) -> Outcome {
        match self.refresh_policy() {
            Ok(true) => {}
            Ok(false) => {
                // The policy in force is satisfied by the fresh measurement, so this
                // candidate no longer qualifies and the pass stops before it.
                if self.stop.is_none() {
                    self.stop = Some("target_reached");
                }
                return Outcome::Skipped(Skip::new(
                    "policy_changed",
                    format!(
                        "the policy in force is already satisfied ({} free of {}), so the root is kept",
                        self.current.free_bytes,
                        self.target()
                    ),
                ));
            }
            Err(error) => return Outcome::Stop(error),
        }
        let target = match self.target_for(candidate) {
            ResolvedTarget::Ready(target) => *target,
            ResolvedTarget::Skipped(skip) => return Outcome::Skipped(skip),
            ResolvedTarget::Stop(error) => return Outcome::Stop(error),
        };
        let unit = deletion_unit(&self.record, &target, candidate, &self.plan_sha256);
        // A pending intent covering **this** object keeps it and is reported as
        // `pending_intent`, before the deletion's own preflight could describe it as
        // merely absent: an unsettled delete has already renamed the root into its
        // quarantine leaf, and the operator needs to know a replay owes it. A
        // pending intent over another path is a different object's business (ruling
        // `698d9ba3`) and reaches no other candidate.
        let pending = state::read_intents_with_deadline(self.paths, INTENT_ROWS, Deadline::after(self.wall_deadline.saturating_duration_since(Instant::now()).min(state::READ_DEADLINE))).map_err(|error| {
            Outcome::Stop(
                Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the intent journal could not be read, so this pass stopped before an effect: {error}"
                    ),
                ),
            )
        });
        match pending {
            Err(outcome) => return outcome,
            Ok(intents) => {
                if PendingIntent::pending(&intents.records)
                    .iter()
                    .any(|pending| pending.overlaps(&unit))
                {
                    return Outcome::Skipped(Skip::new(
                        "pending_intent",
                        format!(
                            "a pending intent covers the root `{}`, so it is kept until a replay settles it",
                            candidate.name
                        ),
                    ));
                }
            }
        }
        let remaining = self
            .effect_deadline
            .saturating_duration_since(Instant::now());
        let timeout = self.bounds.root_deadline.min(remaining);
        let root_deadline = Instant::now() + timeout;
        let bounds = DeleteBounds {
            timeout,
            deadline: Some(root_deadline),
            ..self.delete_bounds
        };
        let generation = self.policy_generation;
        let now_ms = self.now_ms;
        let wall = remaining;
        let refusal: RefCell<Option<Skip>> = RefCell::new(None);
        let refuse = |skip: Skip| {
            *refusal.borrow_mut() = Some(skip.clone());
            skip.error()
        };
        // The effect step's eligibility check runs inside the deletion, under the
        // volume and root locks it holds, so every fact it reads describes the
        // object the deletion is about to rename.
        let paths = self.paths;
        let dirs = self.dirs;
        let facts = self.facts;
        let mounts = self.mounts;
        let observer = self.observer;
        let stored = target.document.as_ref();
        let authorize = |fresh: &VolumeRecord| {
            reverify_candidate(
                paths, dirs, facts, mounts, observer, fresh, generation, &target, candidate, &unit,
                now_ms, wall,
            )
            .map_err(&refuse)
        };
        let result = delete::delete_unit_under_volume_lock(
            self.paths,
            self.dirs,
            self.facts,
            self.mounts,
            self.observer,
            &target.declaration,
            stored,
            &unit,
            bounds,
            match self.volume_lock.as_ref() {
                Some(lock) => lock,
                None => {
                    return Outcome::Stop(Error::new(
                        ErrorKind::UnknownState,
                        "the volume lock was lost",
                    ))
                }
            },
            Some(&authorize),
            &state::IntentEvidence::none(),
        );
        match result {
            Ok(report) if report.is_deleted() => {
                Outcome::Deleted(Box::new(pruned_row(candidate, &report)))
            }
            Ok(report) => Outcome::Stop(self.pending_intent(report)),
            Err(error) => {
                let skip = refusal.borrow_mut().take();
                self.classify(error, skip)
            }
        }
    }

    /// The folder and declaration one planned row is acted on with, re-read now.
    fn target_for(&self, candidate: &SpaceCandidateRow) -> ResolvedTarget {
        if candidate.area == scratch_label() {
            // Barge's scratch folder is Barge's own: it resolves from the record's
            // override or the default, never from a stored declaration. A scratch
            // root may live in a project folder one level down, so the folder this
            // candidate is acted in is the one its planned path proves.
            let resolved = match area::resolve_feature(
                self.dirs,
                self.facts,
                self.mounts,
                self.observer,
                &self.record,
                area::Feature::Scratch,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    return ResolvedTarget::Stop(Error::new(
                        error.kind(),
                        format!(
                            "the scratch folder could not be resolved again, so this pass stopped: {error}"
                        ),
                    ))
                }
            };
            let relative = candidate
                .path
                .parent()
                .and_then(|parent| parent.strip_prefix(&resolved.path).ok())
                .and_then(Path::to_str);
            let Some(relative) = relative else {
                return ResolvedTarget::Skipped(Skip::new(
                    "identity_changed",
                    format!(
                        "the planned scratch root `{}` is not inside the scratch folder this configuration resolves (`{}`), so it is kept",
                        candidate.name,
                        resolved.path.display()
                    ),
                ));
            };
            let folder = if relative.is_empty() {
                resolved.folder
            } else {
                format!("{}/{relative}", resolved.folder)
            };
            return ResolvedTarget::Ready(Box::new(Target {
                declaration: scratch::scratch_declaration(&self.record, &folder),
                document: None,
                folder,
            }));
        }
        let Some((namespace, area_name)) = candidate.area.split_once('/') else {
            return ResolvedTarget::Stop(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the plan's area `{}` is not `<namespace>/<area>`, so this run cannot resolve it",
                    candidate.area
                ),
            ));
        };
        let document = match area::show(self.paths, namespace, area_name) {
            Ok(Some(document)) => document,
            Ok(None) => {
                return ResolvedTarget::Skipped(Skip::new(
                    "policy_changed",
                    format!(
                        "the declaration {} was withdrawn before this root's effect, so the root is kept",
                        candidate.area
                    ),
                ))
            }
            Err(error) => {
                return ResolvedTarget::Stop(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the declaration {}/{} could not be read again, so this pass stopped: {error}",
                        namespace, area_name
                    ),
                ))
            }
        };
        let declaration = document.declaration();
        if declaration.reserve != ReserveParticipation::Counts {
            return ResolvedTarget::Skipped(Skip::new(
                "policy_changed",
                format!(
                    "the area {}/{} no longer participates (`{}`), so its root is kept",
                    namespace,
                    area_name,
                    reserve_name(declaration.reserve)
                ),
            ));
        }
        let folder = match area::effective_folder(&self.record, &declaration) {
            Ok(folder) => folder,
            Err(error) => {
                return ResolvedTarget::Stop(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the area {}/{}'s folder could not be resolved again, so this pass stopped: {error}",
                        namespace, area_name
                    ),
                ))
            }
        };
        ResolvedTarget::Ready(Box::new(Target {
            declaration,
            document: Some(document),
            folder,
        }))
    }

    /// A pending intent left by a candidate's own deletion stops the pass: its
    /// quarantine leaf and intent are the recovery evidence, and no later effect
    /// may run over an unsettled one.
    fn pending_intent(&self, report: DeleteReport) -> Error {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "the deletion of `{}` stopped short and left a pending intent, so this pass stopped: {}",
                report.unit_name,
                report
                    .reason
                    .as_deref()
                    .unwrap_or("the deletion did not reach its completion")
            ),
        )
        .with_detail("pending_intent", report.intent_id.into())
        .with_detail("phase", report.phase.as_str().into())
        .with_detail("unit_name", report.unit_name.into())
        .with_detail("quarantine", report.quarantine.display().to_string().into())
    }

    /// Classify one deletion's error. A candidate's own refusal keeps that
    /// candidate; everything else stops the pass.
    fn classify(&mut self, error: Error, skip: Option<Skip>) -> Outcome {
        if let Some(skip) = skip {
            return Outcome::Skipped(skip);
        }
        // A per-root deadline reached before the intent, or a bound inside the
        // deletion, is the pass's budget: the root is kept, the pass stops, and the
        // rows it did not reach are reported with counts.
        let budget = match error.kind() {
            ErrorKind::ResourceBound => true,
            ErrorKind::RefusedBoundary => {
                error
                    .detail()
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    == Some("deadline_before_intent")
            }
            _ => false,
        };
        if budget {
            self.stop = Some("budget_exhausted");
            return Outcome::Skipped(Skip::new(
                "budget_exhausted",
                format!(
                    "the root's own deadline or bound was reached before its effect, so it is kept: {}",
                    error.message()
                ),
            ));
        }
        match error.kind() {
            // A root a consumer holds, or one whose custody the scan could not
            // complete, is skipped and reported — never "attempted and failed".
            ErrorKind::CustodyNotProven => Outcome::Skipped(Skip::new(
                "held",
                format!(
                    "custody is not proven, so the root is kept and reported: {}",
                    error.message()
                ),
            )),
            ErrorKind::Locked => Outcome::Skipped(Skip::new(
                "locked",
                format!(
                    "the root's lock is held by a live owner, so the root is kept: {}",
                    error.message()
                ),
            )),
            ErrorKind::ForeignOwner | ErrorKind::RefusedContainment => Outcome::Skipped(Skip::new(
                "refused",
                format!("the deletion refused this root: {}", error.message()),
            )),
            // The request's own well-formedness is never one candidate's business.
            ErrorKind::Usage => Outcome::Stop(error),
            ErrorKind::UnknownState => {
                let reason = error
                    .detail()
                    .get("reason")
                    .and_then(serde_json::Value::as_str);
                match reason {
                    Some("unit_absent" | "unit_parent_absent" | "unit_kind_changed") => {
                        Outcome::Skipped(Skip::new(
                            "identity_changed",
                            format!(
                                "the root is absent, renamed or another kind since the plan, so nothing was deleted: {}",
                                error.message()
                            ),
                        ))
                    }
                    _ => Outcome::Stop(error),
                }
            }
            // A durability that stopped permitting pruning and a declaration that
            // left the pass are not one root's business: the pass stops.
            _ => Outcome::Stop(error),
        }
    }

    /// Name what this pass already deleted on a failure that stops it, so an
    /// `unknown_state` refusal is never silent about the effects before it.
    fn annotate(&self, error: Error) -> Error {
        let ids: Vec<&str> = self.pruned.iter().map(|row| row.root_id.as_str()).collect();
        with_replay_detail(error, &self.replay)
            .with_detail("replay", serde_json::json!(&self.replay))
            .with_detail("pruned_root_ids", serde_json::json!(ids))
            .with_detail("pruned", self.pruned.len().into())
            .with_detail("reclaimed_bytes", self.reclaimed().into())
            .with_detail("deleted_bytes", self.deleted_bytes.into())
            .with_detail("batches", self.batches.into())
            .with_detail("volume_uuid", self.record.volume_uuid.as_str().into())
    }

    /// The `barge.space.prune/1` body this pass earned, closed at `started` plus the
    /// duration the pass actually took.
    fn into_report(self, started: Instant) -> SpacePruneReport {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let policy_generation = self.policy_generation;
        let target_bytes = self.target();
        let state = state_name(&self.current, &self.policy).to_owned();
        let pruned_total = self.pruned.len();
        let held_total = self.held.len();
        let unreached_total = self.unreached.len();
        let effect = SpacePruneEffect {
            started_ms: self.started_ms,
            // The pass's own clock advanced by what it measured, so the two times
            // are the same clock and `finished_ms` is never before `started_ms`.
            finished_ms: self.started_ms.saturating_add(elapsed_ms),
            before: self.before.clone(),
            after: self.current.clone(),
            policy: self.policy,
            policy_generation,
            target_bytes,
            batches: self.batches,
            rounds: self.rounds,
            pruned: RowSet::bounded(self.pruned, pruned_total, ROOT_ROWS),
            held: RowSet::bounded(self.held, held_total, ROOT_ROWS),
            deleted_bytes: self.deleted_bytes,
            reclaimed_bytes: self
                .current
                .free_bytes
                .saturating_sub(self.before.free_bytes),
            stop: self.stop.unwrap_or("nothing_eligible").to_owned(),
            flush: self.flush_row,
            live_plan_truncated: self.live_plan_truncated,
            live_plan_unreached: self.live_plan_unreached,
            replay: Some(SpaceReplayRow::from(&self.replay)),
            not_reached: RowSet::bounded(self.unreached, unreached_total, ROOT_ROWS),
        };
        let truncated =
            effect.pruned.truncated || effect.held.truncated || effect.not_reached.truncated;
        SpacePruneReport {
            preview: false,
            planned_ms: self.now_ms,
            volume_uuid: self.plan.volume_uuid.clone(),
            mount_point: self.mount,
            // The plan's own policy and generation: the bytes the caller
            // presented. The policy in force when the pass stopped is beside them
            // in `effect`, so a generation change is visible rather than hidden.
            policy: self.plan.policy.clone(),
            policy_generation: self.plan.policy_generation,
            measurement: self.current,
            state,
            target_bytes,
            candidates: self.plan.candidates.count,
            plan_sha256: self.plan_sha256,
            plan: self.plan,
            // A real run does not re-plan: the participation and kept rows belong
            // to the preview that authorised it, and are reported there.
            participation: RowSet::empty(),
            kept: RowSet::empty(),
            examined: 0,
            truncated,
            truncated_reason: truncated.then(|| {
                "a pass bound cut a reported row list; every count reports the real total"
                    .to_owned()
            }),
            unreached: effect.not_reached.total,
            effect: Some(effect),
        }
    }
}

/// The effect step's re-verification: the full custody set, read under the volume
/// and root locks the deletion holds.
///
/// It refuses with the reason code the report carries, and the deletion stops
/// before its intent is written, so a root that changed is kept whole. Every fact
/// is read fresh: the registry generation, the area's participation and
/// declaration, the marker and its digest, the root's device and inode, its
/// pressure idleness (holds, activity and, for scratch, its conclusion), its
/// durable identity record and its pending intents. The same-uid descriptor scan
/// and the volume re-verification are custody's and the deletion's own, and they
/// run after this check inside the same locked window.
#[allow(clippy::too_many_arguments)]
fn reverify_candidate(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    fresh: &VolumeRecord,
    generation: u64,
    target: &Target,
    candidate: &SpaceCandidateRow,
    unit: &DeletionUnit,
    now_ms: u64,
    wall: Duration,
) -> std::result::Result<(), Skip> {
    // No pending intent may cover this root, and that is decided **before** anything
    // about the root's current shape. An earlier run's unsettled delete of this
    // object has already renamed it into its quarantine leaf, so its folder would
    // read as "renamed or absent" and hide the fact an operator needs: a replay owes
    // this object. A pending intent over another path is a different object's
    // business and never reaches this candidate (ruling `698d9ba3`).
    let intents = state::read_intents_with_deadline(
        paths,
        INTENT_ROWS,
        Deadline::after(wall.min(state::READ_DEADLINE)),
    )
    .map_err(|error| {
        Skip::new(
            "pending_intent",
            format!("the intent journal could not be read, so the root is kept: {error}"),
        )
    })?;
    if PendingIntent::pending(&intents.records)
        .iter()
        .any(|pending| pending.overlaps(unit))
    {
        return Err(Skip::new(
            "pending_intent",
            format!(
                "a pending intent covers the root `{}`, so it is kept until a replay settles it",
                candidate.name
            ),
        ));
    }
    if fresh.generation != generation {
        return Err(Skip::new(
            "policy_changed",
            format!(
                "the registry record changed again before this root's effect (generation {generation} to {}), so the root is re-evaluated against the new policy",
                fresh.generation
            ),
        ));
    }
    // The policy must still be declared: a volume whose policy was removed is
    // re-evaluated, never acted on under the plan's stale thresholds.
    fresh.declared_space().map_err(|error| {
        Skip::new(
            "policy_changed",
            format!(
                "the volume's declared space policy could not be read again, so the root is kept: {error}"
            ),
        )
    })?;
    if let Some(document) = &target.document {
        let Some(stored) =
            area::show(paths, &document.namespace, &document.area).map_err(|error| {
                Skip::new(
                    "policy_changed",
                    format!(
                    "the declaration {}/{} could not be read again, so the root is kept: {error}",
                    document.namespace, document.area
                ),
                )
            })?
        else {
            return Err(Skip::new(
                "policy_changed",
                format!(
                    "the declaration {}/{} was withdrawn before this root's effect, so the root is kept",
                    document.namespace, document.area
                ),
            ));
        };
        if stored.declaration() != target.declaration {
            return Err(Skip::new(
                "policy_changed",
                format!(
                    "the declaration {}/{} changed since the plan, so the root is kept",
                    document.namespace, document.area
                ),
            ));
        }
    }
    let resolved = area::resolve(
        dirs,
        facts,
        mounts,
        observer,
        fresh,
        &target.declaration,
        target.document.as_ref(),
    )
    .map_err(|error| {
        Skip::new(
            "identity_changed",
            format!("the area could not be resolved again, so the root is kept: {error}"),
        )
    })?;
    if resolved.folder != unit.folder || resolved.root_dir != unit.root_dir {
        return Err(Skip::new(
            "identity_changed",
            format!(
                "the area's folder moved to `{}` since the plan, so the root is kept",
                resolved.folder
            ),
        ));
    }
    let listing = area::list_roots_with(
        dirs,
        observer,
        paths,
        &resolved,
        ROOT_ROWS,
        area::ActivityMode::Floor,
    )
    .map_err(|error| {
        Skip::new(
            "identity_changed",
            format!("the area could not be listed again, so the root is kept: {error}"),
        )
    })?;
    let Some(row) = listing.roots.iter().find(|row| row.name == candidate.name) else {
        return Err(Skip::new(
            "identity_changed",
            format!(
                "the root `{}` is no longer in its folder, so it was renamed or removed since the plan; nothing was deleted",
                candidate.name
            ),
        ));
    };
    if row.root_id.as_deref() != Some(candidate.root_id.as_str()) {
        return Err(Skip::new(
            "identity_changed",
            format!(
                "the directory `{}` now holds another root than the plan named, so it is kept",
                candidate.name
            ),
        ));
    }
    if row.state != MarkerStateName::Owned {
        return Err(Skip::new(
            "identity_changed",
            format!(
                "the root `{}`'s marker no longer verifies, so it is kept",
                candidate.name
            ),
        ));
    }
    if row.device != Some(candidate.device)
        || row.inode != Some(candidate.inode)
        || row.marker_digest.as_deref() != Some(candidate.marker_digest.as_str())
    {
        return Err(Skip::new(
            "identity_changed",
            format!(
                "the root `{}`'s identity or marker bytes changed since the plan, so the root is kept",
                candidate.name
            ),
        ));
    }
    if !row.space_idle.is_idle() {
        let reason = row.space_idle.reason.map_or_else(
            || "not_idle".to_owned(),
            |reason| reason.as_str().to_owned(),
        );
        let code = match reason.as_str() {
            "hold_live" | "hold_unknown" => "held",
            _ => "not_idle",
        };
        return Err(Skip::new(
            code,
            format!(
                "the root `{}` is no longer idle under the pressure matrix ({reason}), so it is kept",
                candidate.name
            ),
        ));
    }
    // One handle to the root, under the lock the deletion holds: it serves the
    // scratch predicate and the durable identity binding below, so both read the
    // object this effect is about to rename.
    let handle = dirs.open_path(&row.configured_path).map_err(|error| {
        Skip::new(
            "identity_changed",
            format!(
                "the root `{}` could not be opened again, so it is kept: {error}",
                candidate.name
            ),
        )
    })?;
    if candidate.area == scratch_label() {
        let eligibility = scratch::pressure_eligible(
            dirs,
            observer,
            paths,
            fresh,
            &candidate.name,
            &row.configured_path,
            &handle,
            DeviceId::new(candidate.device),
            now_ms,
            wall,
        )
        .map_err(|error| {
            Skip::new(
                "not_idle",
                format!(
                    "the scratch root `{}`'s expiry predicate could not be decided again, so it is kept: {error}",
                    candidate.name
                ),
            )
        })?;
        if !eligibility.eligible {
            return Err(Skip::new(
                "not_idle",
                format!(
                    "the scratch root `{}` is no longer pressure-eligible ({}), so it is kept",
                    candidate.name, eligibility.reason
                ),
            ));
        }
        if eligibility
            .marker_digest
            .as_deref()
            .is_some_and(|digest| digest != candidate.marker_digest)
        {
            return Err(Skip::new(
                "identity_changed",
                format!(
                    "the scratch root `{}`'s marker bytes changed since the plan, so it is kept",
                    candidate.name
                ),
            ));
        }
    }
    // The durable record must agree with **this** observation when the root has one.
    // A root that predates the record has none, and that absence is not a
    // disagreement; an unreadable or disagreeing record keeps the root rather than
    // being resolved in either direction. The comparison is this candidate's, never
    // the record's own path: a copied root that reuses another root's id in another
    // folder is exactly the case the record alone would verify and this comparison
    // refuses.
    let uid = observer.effective_uid().map_err(|error| {
        Skip::new(
            "identity_changed",
            format!("this account's effective uid is unreadable: {error}"),
        )
    })?;
    let record = match identity::read(paths, dirs, &candidate.root_id, uid) {
        Ok(None) => None,
        Ok(Some(record)) => Some(record),
        Err(error) => {
            return Err(Skip::new(
                "identity_changed",
                format!(
                "the root `{}`'s durable identity record could not be read, so it is kept: {error}",
                candidate.name
            ),
            ))
        }
    };
    if let Some(record) = record {
        let Some(marker) = observed_marker(
            dirs,
            &handle,
            &resolved.volume_uuid,
            &resolved.mount_point,
            &row.configured_path,
            uid,
            now_ms,
            candidate,
        ) else {
            return Err(Skip::new(
                "identity_changed",
                format!(
                    "the root `{}`'s marker could not be re-read for its identity record, so it is kept",
                    candidate.name
                ),
            ));
        };
        let dir_facts = dirs.dir_facts(&handle).map_err(|error| {
            Skip::new(
                "identity_changed",
                format!(
                    "the root `{}`'s own facts could not be read, so it is kept: {error}",
                    candidate.name
                ),
            )
        })?;
        let record_uuid = match VolumeUuid::parse(&record.volume_uuid) {
            Ok(uuid) => uuid,
            Err(error) => {
                return Err(Skip::new(
                    "identity_changed",
                    format!(
                        "the root `{}`'s identity record names a volume that is not a UUID, so it is kept: {error}",
                        candidate.name
                    ),
                ))
            }
        };
        let matching_mounts = match facts.mount_census(&record_uuid) {
            Ok(mounts) => mounts.len(),
            Err(error) => {
                return Err(Skip::new(
                    "identity_changed",
                    format!(
                        "volume {}'s mount census is unavailable, so the root `{}`'s identity is unknown: {error}",
                        record.volume_uuid, candidate.name
                    ),
                ))
            }
        };
        let verdict = identity::compare(
            &record,
            &identity::RootObservation {
                matching_mounts,
                // The live volume is the one this pass verified, never the record's
                // uuid compared with itself: a record that names another volume is a
                // mismatch, not a match.
                volume_uuid: resolved.volume_uuid.as_str().to_owned(),
                mount: resolved.mount_point.clone(),
                device: candidate.device,
                inode: candidate.inode,
                root_uid: dir_facts.uid,
                effective_uid: uid,
                root_id: marker.root_id,
                nonce: marker.nonce,
                mount_relative_path: relative_to_mount(&row.configured_path, &resolved.mount_point),
                kind: marker.kind,
                area: marker.area,
            },
        );
        if let IdentityVerdict::Unknown { reason } = verdict {
            return Err(Skip::new(
                "identity_changed",
                format!(
                    "the root `{}`'s durable identity record does not describe this observation ({}), so it is kept",
                    candidate.name,
                    identity_reason(reason)
                ),
            ));
        }
    }
    Ok(())
}

/// The stage 1 deletion unit one planned space candidate is: a Barge-owned
/// directory root with its planned identity and its own grant.
fn deletion_unit(
    record: &VolumeRecord,
    target: &Target,
    candidate: &SpaceCandidateRow,
    digest: &str,
) -> DeletionUnit {
    DeletionUnit {
        volume_uuid: record.volume_uuid.clone(),
        root_dir: record.root_dir.clone(),
        folder: target.folder.clone(),
        parent: String::new(),
        name: candidate.name.clone(),
        kind: UnitKind::Directory,
        device: DeviceId::new(candidate.device),
        inode: candidate.inode,
        marker: Some(UnitMarker::new(
            MarkerFamily::Barge,
            candidate.marker_digest.clone(),
        )),
        // An adopted root's directory name is not its root id, so the deletion must
        // also take the root-id lock a consumer's `hold` takes; without it a held
        // root could be renamed out from under its holder.
        root_id: (candidate.root_id != candidate.name).then(|| candidate.root_id.clone()),
        grant: UnitGrant::native_root(candidate.name.clone()),
        plan_digest: digest.to_owned(),
    }
}

/// The row one completed deletion earns.
fn pruned_row(candidate: &SpaceCandidateRow, report: &DeleteReport) -> SpacePrunedRow {
    SpacePrunedRow {
        root_id: candidate.root_id.clone(),
        area: candidate.area.clone(),
        name: candidate.name.clone(),
        path: candidate.path.clone(),
        bytes: candidate.bytes,
        last_release_ms: candidate.last_release_ms,
        intent_id: report.intent_id.clone(),
        entries_removed: report.entries_removed,
        directories_removed: report.directories_removed,
        marker_removed: report.marker_removed,
    }
}

/// Barge's own scratch area, as a plan spells it.
fn scratch_label() -> String {
    format!("{}/{}", scratch::SCRATCH_NAMESPACE, scratch::SCRATCH_AREA)
}

/// A participation's name, as a refusal spells it.
const fn reserve_name(reserve: ReserveParticipation) -> &'static str {
    match reserve {
        ReserveParticipation::Counts => "counts",
        ReserveParticipation::Excluded => "excluded",
    }
}

/// Re-read the marker of the root an effect step is about to rename, under the lock
/// the deletion holds, for the identity record's binding. It returns the owned
/// marker when the root is positively Barge's own.
#[allow(clippy::too_many_arguments)]
fn observed_marker(
    dirs: &dyn DirOps,
    handle: &crate::os::DirHandle,
    volume_uuid: &VolumeUuid,
    mount_point: &Path,
    resolved_path: &Path,
    uid: u32,
    now_ms: u64,
    candidate: &SpaceCandidateRow,
) -> Option<crate::area::RootMarker> {
    let expectation = area::MarkerExpectation {
        volume_uuid,
        resolved_path,
        mount_point: Some(mount_point),
        effective_uid: uid,
        now_ms,
    };
    let (marker, digest) = area::read_marker_with_digest(dirs, handle, &expectation);
    if digest.as_deref() != Some(candidate.marker_digest.as_str()) {
        return None;
    }
    match marker {
        area::MarkerState::Owned(marker) if marker.root_id == candidate.root_id => Some(*marker),
        _ => None,
    }
}

/// One path's form below a mount point, as a durable identity record spells it.
fn relative_to_mount(path: &Path, mount: &Path) -> String {
    let Ok(relative) = path.strip_prefix(mount) else {
        return String::new();
    };
    relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<&str>>()
        .join("/")
}

/// A durable identity refusal's reason code, which is the design's reason table.
const fn identity_reason(reason: IdentityReason) -> &'static str {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    use super::{prune, SpacePruneBounds, SpacePruneRunRequest};
    use crate::area::{
        self, AreaDeclaration, HoldRequest, HoldScope, ReserveParticipation, RootKind,
    };
    use crate::contract::{RowSet, INTENT_ROWS};
    use crate::custody::PendingIntent;
    use crate::delete::DeleteBounds;
    use crate::digest::sha256_hex;
    use crate::error::ErrorKind;
    use crate::os::{
        Flush, OpenDescriptor, SpaceFacts, SpaceMeasurement, VerifiedVolume, VolumeUuid,
    };
    use crate::registry::{self, Durability, SpacePolicy, VolumeRecord};
    use crate::space::{
        canonical_plan_bytes, plan, read_prune_report, SpacePruneReport, SpacePruneRequest,
        SPACE_PLAN_WALL,
    };
    use crate::state::{self, Deadline, LockSet};
    use crate::testkit::{FakeObserver, FakeVolume, Faults, VolumeFixture};

    /// The reserve and floor every harness registers: free below the target, so a
    /// pass has something to reclaim.
    const RESERVE: u64 = 16;
    const FLOOR: u64 = 4;

    /// One measurement script: the pass takes these free-byte figures in order and
    /// repeats the last, so a test states exactly what each decision saw.
    struct Meter {
        script: RefCell<VecDeque<u64>>,
        last: Cell<u64>,
        calls: Cell<usize>,
    }

    impl Meter {
        fn new(values: &[u64]) -> Self {
            let last = values.last().copied().unwrap_or(RESERVE);
            Self {
                script: RefCell::new(values.iter().copied().collect()),
                last: Cell::new(last),
                calls: Cell::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    impl SpaceFacts for Meter {
        fn measure_space(&self, volume: &VerifiedVolume) -> crate::error::Result<SpaceMeasurement> {
            self.calls.set(self.calls.get().saturating_add(1));
            let free = self
                .script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.last.get());
            self.last.set(free);
            SpaceMeasurement::from_blocks(
                Some(1),
                Some(free),
                Some(0),
                Some(100),
                1,
                volume.identity().device,
            )
        }
    }

    /// A flush that succeeds or fails on demand, and counts its calls.
    struct Flusher {
        fail: Cell<bool>,
        calls: Cell<usize>,
    }

    impl Flusher {
        fn ok() -> Self {
            Self {
                fail: Cell::new(false),
                calls: Cell::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }

        fn failing() -> Self {
            Self {
                fail: Cell::new(true),
                calls: Cell::new(0),
            }
        }
    }

    impl Flush for Flusher {
        fn flush_volume(
            &self,
            _volume: &VerifiedVolume,
            _deadline: Instant,
        ) -> crate::error::Result<()> {
            self.calls.set(self.calls.get().saturating_add(1));
            if self.fail.get() {
                return Err(crate::error::Error::new(
                    ErrorKind::UnknownState,
                    "an injected flush failure",
                ));
            }
            Ok(())
        }
    }

    /// One fixture volume with a declared policy, a declared area and its roots.
    struct Harness {
        fixture: VolumeFixture,
        record: VolumeRecord,
        facts: FakeVolume,
        observer: FakeObserver,
        now_ms: u64,
    }

    impl Harness {
        fn new(label: &str, reserve: u64, floor: u64) -> Self {
            let fixture = VolumeFixture::under_scratch(label);
            let facts = fixture.facts();
            let record = Self::register(&fixture, reserve, floor);
            let observer = fixture.observer();
            area::declare(
                &fixture.state,
                &Self::declaration("pool", "example/pool", ReserveParticipation::Counts),
                1,
            )
            .expect("the area declares");
            let now_ms = state::now_ms().saturating_add(60_000);
            Self {
                fixture,
                record,
                facts,
                observer,
                now_ms,
            }
        }

        fn register(fixture: &VolumeFixture, reserve: u64, floor: u64) -> VolumeRecord {
            let mut record = fixture.record.clone();
            record.space = Some(SpacePolicy {
                reserve_bytes: reserve,
                floor_bytes: floor,
                declared_by: "fixture".to_owned(),
                declared_ms: 1,
            });
            fixture.register_record(&record);
            registry::read(&fixture.state)
                .expect("the registry reads")
                .volume(&fixture.uuid)
                .cloned()
                .expect("the fixture volume is registered")
        }

        fn declaration(
            area_name: &str,
            folder: &str,
            reserve: ReserveParticipation,
        ) -> AreaDeclaration {
            AreaDeclaration {
                namespace: "example".to_owned(),
                area: area_name.to_owned(),
                folder: folder.to_owned(),
                overrides: std::collections::BTreeMap::new(),
                absent: area::AbsentPolicy::Fail {
                    attach: "the fixture volume".to_owned(),
                },
                prune: area::PrunePolicy::None,
                reserve,
            }
        }

        /// Declare one more area.
        #[allow(dead_code)]
        fn declare(&self, area_name: &str, reserve: ReserveParticipation) {
            area::declare(
                &self.fixture.state,
                &Self::declaration(area_name, &format!("example/{area_name}"), reserve),
                1,
            )
            .expect("the area declares");
        }

        /// Create one idle, released root in an area and return its id and path.
        fn seed(&self, area_name: &str, released_at: u64) -> (String, std::path::PathBuf) {
            let declaration = Self::declaration(
                area_name,
                &format!("example/{area_name}"),
                ReserveParticipation::Counts,
            );
            let (root, _) =
                self.fixture
                    .create_root(&self.record, &declaration, RootKind::AreaRoot);
            VolumeFixture::payload(&root.path, "payload.bin", &[7u8; 4096]);
            self.release_at(&root.root_id, released_at, released_at.saturating_sub(100));
            (root.root_id, root.path)
        }

        /// Record one root's release as account state, exactly as a release
        /// leaves it: the release publishes its fact first (T3's order), and this
        /// writes that fact directly so a fixture does not need a hold round trip.
        fn release_at(&self, root_id: &str, at: u64, created: u64) {
            let path = self
                .fixture
                .state
                .activity_path(root_id)
                .expect("the record path");
            let record = serde_json::json!({
                "schema": "barge.activity/1",
                "root_id": root_id,
                "volume_uuid": self.record.volume_uuid.as_str(),
                "last_release_ms": at,
                "first_created_ms": created,
                "releases": 1,
                "generation": 1,
                "recorded_ms": at,
            });
            std::fs::create_dir_all(path.parent().expect("the activity directory"))
                .expect("the activity directory is made");
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&record).expect("the record encodes"),
            )
            .expect("the record is written");
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .expect("the record is private");
        }

        /// One adopted root: a directory whose marker proves a root id that is not
        /// its name. Returns the marker's root id, its path and its nonce.
        fn seed_adopted(
            &self,
            area_name: &str,
            name: &str,
            released_at: u64,
        ) -> (String, std::path::PathBuf, String) {
            use crate::area::{MarkerArea, MarkerWrite, RootOrigin};
            let folder = format!("example/{area_name}");
            let path = self.fixture.root.join(&folder).join(name);
            std::fs::create_dir_all(&path).expect("the adopted root's directory is made");
            VolumeFixture::payload(&path, "payload.bin", &[5u8; 2048]);
            let platform = self.fixture.platform();
            let dirs: &dyn crate::os::DirOps = &platform;
            let facts: &dyn crate::os::ProcessObserver = &platform;
            let handle = dirs.open_path(&path).expect("the adopted root opens");
            let uid = facts.effective_uid().expect("this account's uid");
            let marker = area::write_marker(
                &platform,
                &handle,
                &MarkerWrite {
                    root_id: None,
                    volume_uuid: &self.record.volume_uuid,
                    resolved_path: &path,
                    mount_point: &self.fixture.root,
                    area: MarkerArea::identity("example", area_name),
                    kind: RootKind::AreaRoot,
                    origin: RootOrigin::AdoptedExisting,
                },
                uid,
            )
            .expect("the adopted root's marker is written");
            self.release_at(
                &marker.root_id,
                released_at,
                released_at.saturating_sub(100),
            );
            (marker.root_id, path, marker.nonce)
        }

        /// Publish one durable `barge.root.identity/1` record by hand, for the
        /// counterfactual where the record does not describe the candidate.
        fn write_identity(
            &self,
            root_id: &str,
            inode: u64,
            nonce: &str,
            mount_relative_path: &str,
            area_name: &str,
        ) {
            let roots = self.fixture.state.root().join("roots");
            let dir = roots.join(root_id);
            std::fs::create_dir_all(&dir).expect("the identity directory is made");
            std::fs::set_permissions(&roots, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .expect("the identity root is private");
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .expect("the identity directory is private");
            let record = serde_json::json!({
                "schema": "barge.root.identity/1",
                "root_id": root_id,
                "nonce": nonce,
                "volume_uuid": self.record.volume_uuid.as_str(),
                "mount_relative_path": mount_relative_path,
                "inode": inode,
                "device_at_record": self.fixture.device.get(),
                "mount_at_record": self.fixture.root.to_str().expect("the mount path is UTF-8"),
                "kind": "area_root",
                "area": {"namespace": "example", "area": area_name},
                "origin": "created",
                "marker_generation_at_record": 1,
                "recorded_ms": 1,
            });
            let file = dir.join("identity.json");
            std::fs::write(
                &file,
                serde_json::to_vec_pretty(&record).expect("the record encodes"),
            )
            .expect("the identity record is written");
            std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .expect("the identity record is private");
        }

        /// Publish a registry of every record one account holds, so two volumes can
        /// share one journal.
        fn register_all(&self, records: &[VolumeRecord]) {
            use crate::registry::{RegistryDocument, RegistryWriter};
            self.fixture.state.initialize().expect("the layout exists");
            let mut locks = LockSet::with_deadline(
                self.fixture.state.clone(),
                Deadline::after(Duration::from_secs(5)),
            );
            let lock = locks.lock_registry().expect("the registry lock");
            RegistryWriter::new(&self.fixture.state)
                .publish(&lock, &RegistryDocument::new(records.to_vec()))
                .expect("the registry is written");
        }

        fn hold(&self, root_id: &str) {
            area::hold(
                &self.fixture.state,
                &self.observer,
                &HoldRequest {
                    root_id: root_id.to_owned(),
                    holder: "consumer".to_owned(),
                    pid: std::process::id(),
                    scope: HoldScope::Write,
                    purpose: "a live consumer".to_owned(),
                },
            )
            .expect("the hold is written");
        }

        /// The preview and the exact bytes it authorises.
        fn preview(&self) -> (SpacePruneReport, Vec<u8>) {
            self.preview_on(self.fixture.facts(), RESERVE)
        }

        fn preview_on(&self, facts: FakeVolume, free: u64) -> (SpacePruneReport, Vec<u8>) {
            let meter = Meter::new(&[free]);
            let report = plan(
                &self.fixture.state,
                &self.fixture.platform(),
                &facts,
                &self.fixture.mounts(),
                &self.observer,
                &meter,
                &SpacePruneRequest {
                    volume_uuid: &self.record.volume_uuid,
                    now_ms: self.now_ms,
                    wall: SPACE_PLAN_WALL,
                    lock_wait: Duration::from_secs(1),
                },
            )
            .expect("the plan is built");
            let bytes = canonical_plan_bytes(&report.plan).expect("the plan encodes");
            (report, bytes)
        }

        fn request<'a>(
            &'a self,
            digest: &'a str,
            bytes: &'a [u8],
            bounds: SpacePruneBounds,
        ) -> SpacePruneRunRequest<'a> {
            SpacePruneRunRequest {
                volume_uuid: &self.record.volume_uuid,
                plan_digest: digest,
                plan_bytes: bytes,
                now_ms: self.now_ms,
                lock_wait: Duration::from_secs(1),
                delete_bounds: DeleteBounds {
                    retry_pause: Duration::ZERO,
                    ..DeleteBounds::default()
                },
                bounds,
            }
        }

        /// Run the plan's effect form against the real platform.
        fn run(
            &self,
            bytes: &[u8],
            meter: &Meter,
            flusher: &Flusher,
            bounds: SpacePruneBounds,
        ) -> crate::error::Result<SpacePruneReport> {
            let digest = sha256_hex(bytes);
            let request = self.request(&digest, bytes, bounds);
            prune(
                &self.fixture.state,
                &self.fixture.platform(),
                &self.facts,
                &self.fixture.mounts(),
                &self.observer,
                meter,
                flusher,
                &request,
            )
        }

        /// Run through an injected `DirOps`, for the counterfactual faults.
        fn run_with<'a>(
            &'a self,
            dirs: &dyn crate::os::DirOps,
            bytes: &'a [u8],
            meter: &Meter,
            flusher: &Flusher,
            bounds: SpacePruneBounds,
        ) -> crate::error::Result<SpacePruneReport> {
            let digest = sha256_hex(bytes);
            let request = self.request(&digest, bytes, bounds);
            prune(
                &self.fixture.state,
                dirs,
                &self.facts,
                &self.fixture.mounts(),
                &self.observer,
                meter,
                flusher,
                &request,
            )
        }

        fn no_pending_intent(&self) {
            let intents =
                state::read_intents(&self.fixture.state, INTENT_ROWS).expect("the journal reads");
            assert!(
                PendingIntent::pending(&intents.records).is_empty(),
                "no intent is left pending"
            );
        }
    }

    fn encode(report: &SpacePruneReport) -> Vec<u8> {
        let document = serde_json::json!({
            "schema": "barge.space.prune/1",
            "command": "space.prune",
            "ok": true,
            "data": report,
        });
        serde_json::to_vec(&document).expect("the document encodes")
    }

    fn held_reason(report: &SpacePruneReport, root_id: &str) -> Option<String> {
        report
            .effect
            .as_ref()
            .expect("a real run carries its effect")
            .held
            .rows
            .iter()
            .find(|row| row.root_id == root_id)
            .map(|row| row.reason.clone())
    }

    #[test]
    fn a_real_pass_deletes_the_plans_candidates_through_the_stage_one_protocol() {
        let harness = Harness::new("space-prune-effect", RESERVE, FLOOR);
        let (root_a, path_a) = harness.seed("pool", 1_000);
        let (preview, bytes) = harness.preview();
        assert_eq!(preview.candidates, 1);
        assert!(path_a.is_dir());

        // free 4 under the 16 target, then 20 after the batch's flush.
        let meter = Meter::new(&[4, 20]);
        let flusher = Flusher::ok();
        let report = harness
            .run(&bytes, &meter, &flusher, SpacePruneBounds::design())
            .expect("the pass runs");

        assert!(!report.preview);
        let effect = report.effect.as_ref().expect("the effect is reported");
        assert_eq!(effect.pruned.count, 1, "{effect:?}");
        assert_eq!(effect.pruned.rows[0].root_id, root_a);
        assert_eq!(effect.pruned.rows[0].intent_id.len(), 32);
        assert!(effect.pruned.rows[0].marker_removed, "the marker went last");
        assert_eq!(effect.stop, "target_reached");
        assert_eq!(effect.reclaimed_bytes, 16);
        assert_eq!(
            effect.deleted_bytes, preview.plan.candidates.rows[0].bytes,
            "the deleted bytes are the plan's own count, not the reclaim"
        );
        assert_eq!(effect.batches, 1);
        assert_eq!(effect.rounds, 1);
        assert_eq!(effect.flush.attempts, 1);
        assert_eq!(effect.flush.failures, 0);
        assert_eq!(effect.not_reached.count, 0);
        assert!(!path_a.exists(), "the verified root is gone");
        harness.no_pending_intent();
        assert_eq!(meter.calls(), 2, "one authoritative and one post-flush");
        assert_eq!(flusher.calls(), 1, "one flush per batch");
        assert_eq!(sha256_hex(&bytes), report.plan_sha256);
        assert!(!report.needs_human());
        assert!(
            effect.finished_ms >= effect.started_ms,
            "the document's two times are one clock"
        );

        // The document is one `barge.space.prune/1` and reads back through the
        // schema's own reader; a removed field and an unknown major are refused.
        let encoded = encode(&report);
        let read = read_prune_report(&encoded).expect("the real run's document reads");
        assert_eq!(read.effect.expect("the effect").stop, "target_reached");
        let mut missing = serde_json::from_slice::<serde_json::Value>(&encoded).expect("json");
        missing["data"]["effect"]
            .as_object_mut()
            .expect("the effect is an object")
            .remove("stop");
        let broken = serde_json::to_vec(&missing).expect("encode");
        assert_eq!(
            read_prune_report(&broken)
                .expect_err("a removed field is refused")
                .kind(),
            ErrorKind::UnknownState
        );
        let mut later = serde_json::from_slice::<serde_json::Value>(&encoded).expect("json");
        later["schema"] = serde_json::json!("barge.space.prune/2");
        assert_eq!(
            read_prune_report(&serde_json::to_vec(&later).expect("encode"))
                .expect_err("an unknown major is refused")
                .kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn a_pass_already_above_its_target_changes_nothing() {
        let harness = Harness::new("space-prune-above", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let meter = Meter::new(&[20]);
        let report = harness
            .run(&bytes, &meter, &Flusher::ok(), SpacePruneBounds::design())
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "target_reached");
        assert_eq!(effect.pruned.count, 0);
        assert_eq!(effect.batches, 0);
        assert_eq!(meter.calls(), 1);
        assert_eq!(
            effect.not_reached.count, 1,
            "the planned root is accounted for"
        );
        assert!(path.is_dir(), "nothing was deleted");
    }

    #[test]
    fn a_plan_whose_bytes_do_not_match_its_digest_or_its_reader_changes_nothing() {
        let harness = Harness::new("space-prune-refusals", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();

        // The bytes do not hash to the digest the caller presented.
        let mut tampered = bytes.clone();
        tampered.push(b'\n');
        let digest = sha256_hex(&bytes);
        let request = harness.request(&digest, &tampered, SpacePruneBounds::design());
        let error = prune(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[4]),
            &Flusher::ok(),
            &request,
        )
        .expect_err("mismatched bytes are refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(path.is_dir());

        // The bytes match their own digest, but the plan's reader refuses them: an
        // identity digest that does not describe its own device, inode and marker.
        let mut plan: serde_json::Value = serde_json::from_slice(&bytes).expect("the plan json");
        plan["candidates"]["rows"][0]["identity_digest"] = serde_json::json!("0".repeat(64));
        let forged = serde_json::to_vec(&plan).expect("encode");
        let digest = sha256_hex(&forged);
        let request = harness.request(&digest, &forged, SpacePruneBounds::design());
        let error = prune(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[4]),
            &Flusher::ok(),
            &request,
        )
        .expect_err("a forged identity digest is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(path.is_dir(), "nothing was deleted");
        harness.no_pending_intent();
    }

    #[test]
    fn a_busy_volume_lock_refuses_the_real_run_and_changes_nothing() {
        let harness = Harness::new("space-prune-locked", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let held = {
            let mut locks = LockSet::with_deadline(
                harness.fixture.state.clone(),
                Deadline::after(Duration::from_secs(1)),
            );
            locks
                .lock_volume(&harness.record.volume_uuid)
                .expect("the fixture takes the volume lock")
        };
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("a busy volume lock refuses the run");
        assert_eq!(error.kind(), ErrorKind::Locked);
        assert!(path.is_dir(), "nothing was changed");
        harness.no_pending_intent();
        drop(held);

        // The same run succeeds once the lock is free.
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs once the lock is free");
        assert_eq!(report.effect.expect("the effect").pruned.count, 1);
    }

    #[test]
    fn a_held_root_appearing_after_the_plan_is_skipped_and_reported_held() {
        let harness = Harness::new("space-prune-held", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        harness.hold(&root);
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.pruned.count, 0);
        assert_eq!(effect.stop, "nothing_eligible");
        assert_eq!(held_reason(&report, &root).as_deref(), Some("held"));
        assert!(path.is_dir(), "the held root is never touched");
    }

    #[test]
    fn an_older_held_root_is_skipped_while_the_younger_one_is_deleted() {
        let harness = Harness::new("space-prune-lru", RESERVE, FLOOR);
        let (older, older_path) = harness.seed("pool", 1_000);
        let (younger, younger_path) = harness.seed("pool", 2_000);
        let (preview, bytes) = harness.preview();
        assert_eq!(
            preview.plan.candidates.rows[0].root_id, older,
            "the LRU order reaches the older root first"
        );
        harness.hold(&older);
        // free 4, then 20 after the batch: one deletion is enough.
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.held.count, 1);
        assert_eq!(held_reason(&report, &older).as_deref(), Some("held"));
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.pruned.rows[0].root_id, younger);
        assert!(older_path.is_dir(), "the held older root survives");
        assert!(!younger_path.exists(), "the younger root is deleted");
    }

    #[test]
    fn a_root_renamed_or_replaced_after_the_plan_is_skipped_as_identity_changed() {
        let harness = Harness::new("space-prune-identity", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        // The same name, another inode and no marker: the plan's identity is gone.
        std::fs::remove_dir_all(&path).expect("the root is removed");
        std::fs::create_dir(&path).expect("another directory takes the name");

        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.pruned.count, 0);
        assert_eq!(
            held_reason(&report, &root).as_deref(),
            Some("identity_changed")
        );
        assert!(path.is_dir(), "the replacement is never deleted");
    }

    #[test]
    fn a_withdrawn_area_skips_its_candidate_as_policy_changed() {
        let harness = Harness::new("space-prune-withdrawn", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let declaration = area::declaration_path(&harness.fixture.state, "example", "pool")
            .expect("the declaration path");
        std::fs::remove_file(&declaration).expect("the declaration is withdrawn");

        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        assert_eq!(
            held_reason(&report, &root).as_deref(),
            Some("policy_changed")
        );
        assert!(path.is_dir(), "the root is kept");
    }

    #[test]
    fn an_excluded_area_plan_entry_cannot_be_acted_on() {
        // A plan can only name a participating area, so this proves the guard from
        // the other side: a declaration edited to `excluded` after the plan keeps
        // its root instead of deleting it.
        let harness = Harness::new("space-prune-excluded", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        area::declare(
            &harness.fixture.state,
            &Harness::declaration("pool", "example/pool", ReserveParticipation::Excluded),
            2,
        )
        .expect("the declaration is edited");
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        assert_eq!(
            held_reason(&report, &root).as_deref(),
            Some("policy_changed")
        );
        assert!(path.is_dir());
    }

    #[test]
    fn a_raised_reserve_is_adopted_and_the_re_evaluated_candidate_still_goes() {
        let harness = Harness::new("space-prune-reserve-up", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        // Reserve 32, floor 8: the plan's target (16) is stale, and the new policy
        // still needs the prune at free=20. The stale target would stop without
        // deleting, so this distinguishes refreshing before the first decision.
        let record = Harness::register(&harness.fixture, 32, 8);
        assert_ne!(record.generation, harness.record.generation);
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[20, 20, 40]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.policy.reserve_bytes, 32);
        assert_eq!(effect.policy_generation, record.generation);
        assert_eq!(effect.target_bytes, 32);
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.pruned.rows[0].root_id, root);
        assert!(!path.exists());
    }

    #[test]
    fn a_reserve_lowered_below_the_measurement_stops_on_the_fresh_policy() {
        let harness = Harness::new("space-prune-reserve-down", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        // Reserve 4, floor 2: the fresh measurement (20) already satisfies the new
        // policy, so nothing is deleted and no candidate is even attempted.
        Harness::register(&harness.fixture, 4, 2);
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "target_reached");
        assert_eq!(
            effect.policy.reserve_bytes, 4,
            "the fresh policy is adopted"
        );
        assert_eq!(effect.target_bytes, 4);
        assert_eq!(effect.pruned.count, 0);
        assert_eq!(effect.batches, 0, "no effect starts on a satisfied policy");
        assert_eq!(effect.not_reached.count, 1);
        assert_eq!(effect.not_reached.rows[0].reason, "target_reached");
        assert!(path.is_dir(), "the root is kept");
        assert!(
            report.plan.policy.reserve_bytes == RESERVE,
            "the plan's own snapshot stays beside the effect, so the change is visible"
        );
        let _ = root;
    }

    #[test]
    fn an_invalid_registry_stops_the_pass_before_any_effect() {
        let harness = Harness::new("space-prune-bad-registry", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        std::fs::write(harness.fixture.state.registry_path(), b"{not json")
            .expect("the registry is corrupted");
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("an invalid registry stops the pass");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(path.is_dir(), "nothing was deleted");
        harness.no_pending_intent();
    }

    #[test]
    fn a_stop_after_a_deletion_names_what_was_already_deleted() {
        // The first root deletes; the second cannot (a symlink inside it stops the
        // stage 1 walk), so the pass stops with the first root named as deleted.
        let harness = Harness::new("space-prune-stop-detail", RESERVE, FLOOR);
        let (first, first_path) = harness.seed("pool", 1_000);
        let (_second, second_path) = harness.seed("pool", 2_000);
        let target = harness.fixture.dir.join("outside.txt");
        std::fs::write(&target, b"outside the root").expect("the target is written");
        std::os::unix::fs::symlink(&target, second_path.join("escape"))
            .expect("the symlink is made");
        let (preview, bytes) = harness.preview();
        assert_eq!(preview.candidates, 2);
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4, 4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("the second root stops the pass");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.detail().get("pending_intent").is_some());
        let deleted = error
            .detail()
            .get("pruned_root_ids")
            .and_then(|value| value.as_array())
            .expect("the refusal names what it deleted");
        assert!(
            deleted
                .iter()
                .any(|value| value.as_str() == Some(first.as_str())),
            "the deleted root is named: {error}"
        );
        assert!(!first_path.exists());
        assert!(
            std::fs::read(&target).expect("the target reads") == b"outside the root",
            "a symlink is never followed"
        );
    }

    #[test]
    fn a_kept_durability_refuses_the_run_before_any_effect() {
        // A registration holds the volume lock from before its publication, and a
        // pass holds that same lock for its whole length, so a durability change
        // cannot land between two of a pass's candidates. It lands between the plan
        // and the run, and is refused there with nothing changed — the row the
        // failure matrix requires, in the only order a real registration can produce.
        let harness = Harness::new("space-prune-kept", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let mut record = harness.record.clone();
        record.durability = Durability::Kept;
        harness.fixture.register_record(&record);
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("a kept volume is refused before any effect");
        // The failure matrix's durability row: exit 3, not a no-effect refusal at
        // exit 2 — the operator asked for a real prune of data the volume no longer
        // says is expendable, which needs their decision.
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.kind().exit_code(), 3);
        assert!(error.message().contains("kept"));
        assert!(path.is_dir(), "nothing was deleted");
        harness.no_pending_intent();
    }

    #[test]
    fn an_open_descriptor_of_this_uid_inside_the_root_refuses_the_delete() {
        let harness = Harness::new("space-prune-open-fd", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let mut observer = harness.fixture.observer();
        observer.scan = crate::testkit::ScanMode::Open(vec![OpenDescriptor {
            pid: std::process::id(),
            descriptor: 9,
            path: path.join("payload.bin"),
        }]);
        let digest = sha256_hex(&bytes);
        let request = harness.request(&digest, &bytes, SpacePruneBounds::design());
        let report = prune(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &observer,
            &Meter::new(&[4]),
            &Flusher::ok(),
            &request,
        )
        .expect("the pass runs");
        assert_eq!(held_reason(&report, &root).as_deref(), Some("held"));
        assert!(path.is_dir(), "an open descriptor keeps the root");
        let detail = report
            .effect
            .as_ref()
            .expect("the effect")
            .held
            .rows
            .iter()
            .find(|row| row.root_id == root)
            .and_then(|row| row.detail.clone())
            .expect("the refusal reads its evidence");
        assert!(
            detail.contains("custody"),
            "the detail names custody: {detail}"
        );
    }

    #[test]
    fn the_root_lock_is_held_for_the_whole_candidate_effect() {
        let harness = Harness::new("space-prune-root-lock", RESERVE, FLOOR);
        let (root, _path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let refused = std::rc::Rc::new(RefCell::new(None));
        let refused_in_probe = std::rc::Rc::clone(&refused);
        let faults = Faults::new();
        let state = harness.fixture.state.clone();
        let probed = root.clone();
        faults.observing(Box::new(move |_name| {
            if refused_in_probe.borrow().is_some() {
                return;
            }
            let mut locks =
                LockSet::with_deadline(state.clone(), Deadline::after(Duration::from_millis(50)));
            if let Err(error) = locks.lock_root(&probed) {
                *refused_in_probe.borrow_mut() = Some(error.kind());
            }
        }));
        let report = harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        assert_eq!(report.effect.expect("the effect").pruned.count, 1);
        assert_eq!(
            *refused.borrow(),
            Some(ErrorKind::Locked),
            "a consumer's hold waits on the lock the prune holds and then fails locked"
        );
    }

    #[test]
    fn a_flush_failure_gives_unknown_state_with_the_deleted_roots() {
        let harness = Harness::new("space-prune-flush", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::failing(),
                SpacePruneBounds::design(),
            )
            .expect_err("a failed flush is unknown state");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let deleted = error
            .detail()
            .get("pruned_root_ids")
            .and_then(|value| value.as_array())
            .expect("the refusal names what it deleted");
        assert!(deleted
            .iter()
            .any(|value| value.as_str() == Some(root.as_str())));
        assert!(
            !path.exists(),
            "the root was deleted before the flush failed"
        );
    }

    #[test]
    fn a_batch_that_reclaimed_nothing_stops_the_pass() {
        let harness = Harness::new("space-prune-no-reclaim", RESERVE, FLOOR);
        let (first, first_path) = harness.seed("pool", 1_000);
        let (_second, second_path) = harness.seed("pool", 2_000);
        let (_, bytes) = harness.preview();
        // One root per batch, and free 4 before and after the flush: the deletion
        // gave nothing back, so the second root is never attempted.
        let bounds = SpacePruneBounds {
            batch_roots: 1,
            ..SpacePruneBounds::design()
        };
        let report = harness
            .run(&bytes, &Meter::new(&[4, 4]), &Flusher::ok(), bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "space_not_reclaimed");
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.pruned.rows[0].root_id, first);
        assert_eq!(effect.reclaimed_bytes, 0);
        assert_eq!(effect.not_reached.count, 1);
        assert_eq!(effect.not_reached.rows[0].reason, "space_not_reclaimed");
        assert!(!first_path.exists());
        assert!(
            second_path.is_dir(),
            "no further data is deleted for no effect"
        );
        assert!(report.needs_human());
    }

    #[test]
    fn the_roots_bound_reports_what_it_did_not_reach() {
        // A 30-byte target with two roots: the deletion reclaims six bytes, so the
        // pass still needs more and only the roots bound stops it.
        let harness = Harness::new("space-prune-roots-bound", 30, FLOOR);
        harness.seed("pool", 1_000);
        harness.seed("pool", 2_000);
        let (_, bytes) = harness.preview();
        let bounds = SpacePruneBounds {
            roots: 1,
            ..SpacePruneBounds::design()
        };
        let report = harness
            .run(&bytes, &Meter::new(&[4, 10]), &Flusher::ok(), bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "budget_exhausted");
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.not_reached.count, 1);
        assert_eq!(effect.not_reached.rows[0].reason, "budget_exhausted");
        assert!(report.needs_human());
    }

    #[test]
    fn the_batch_bound_reports_what_it_did_not_reach() {
        // A 30-byte target, one root per batch: after one batch (six bytes back) the
        // pass still needs more, and the batch bound ends it with one root left.
        let harness = Harness::new("space-prune-batches-bound", 30, FLOOR);
        harness.seed("pool", 1_000);
        harness.seed("pool", 2_000);
        let (_, bytes) = harness.preview();
        let bounds = SpacePruneBounds {
            batch_roots: 1,
            batches: 1,
            ..SpacePruneBounds::design()
        };
        let report = harness
            .run(&bytes, &Meter::new(&[4, 10]), &Flusher::ok(), bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.batches, 1);
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.stop, "budget_exhausted");
        assert_eq!(effect.not_reached.count, 1);
        assert_eq!(effect.not_reached.rows[0].reason, "budget_exhausted");
    }

    #[test]
    fn the_stop_window_starts_no_effect_and_reports_every_candidate() {
        let harness = Harness::new("space-prune-stop-window", RESERVE, FLOOR);
        harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let bounds = SpacePruneBounds {
            wall: Duration::from_secs(10),
            stop_margin: Duration::from_secs(10),
            ..SpacePruneBounds::design()
        };
        let report = harness
            .run(&bytes, &Meter::new(&[4]), &Flusher::ok(), bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "budget_exhausted");
        assert_eq!(effect.batches, 0);
        assert_eq!(effect.pruned.count, 0);
        assert_eq!(effect.not_reached.count, 1);
        assert_eq!(effect.not_reached.rows[0].reason, "budget_exhausted");
    }

    #[test]
    fn a_spent_per_root_deadline_keeps_the_root_before_an_intent_is_written() {
        let harness = Harness::new("space-prune-root-deadline", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let bounds = SpacePruneBounds {
            root_deadline: Duration::ZERO,
            ..SpacePruneBounds::design()
        };
        let report = harness
            .run(&bytes, &Meter::new(&[4]), &Flusher::ok(), bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "budget_exhausted");
        assert_eq!(effect.pruned.count, 0);
        assert_eq!(
            held_reason(&report, &root).as_deref(),
            Some("budget_exhausted")
        );
        assert!(path.is_dir());
        harness.no_pending_intent();
    }

    #[test]
    fn a_finder_enotempty_after_three_tries_leaves_a_pending_intent_at_exit_three() {
        let harness = Harness::new("space-prune-enotempty", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        // One empty subdirectory, so the injected ENOTEMPTY lands on that
        // directory's removal and takes the three-attempt path.
        std::fs::create_dir(path.join("sub")).expect("the subdirectory is made");
        let (_, bytes) = harness.preview();
        let faults = Faults::new();
        faults.failing_remove_prefix(crate::state::CAPTURE_PREFIX, 3);
        let error = harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("a pending intent stops the pass at exit three");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let pending = error
            .detail()
            .get("pending_intent")
            .and_then(|value| value.as_str())
            .expect("the refusal names the pending intent");
        assert_eq!(pending.len(), 32);
        // The intent and its quarantine leaf are the recovery evidence; the payload
        // is still inside that leaf, never widened or dropped.
        let intents =
            state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("the journal");
        assert_eq!(PendingIntent::pending(&intents.records).len(), 1);
        assert!(
            intents
                .records
                .iter()
                .any(|record| record.intent_id == pending),
            "the named intent is in the journal"
        );
        let leaves: Vec<std::path::PathBuf> = std::fs::read_dir(path.parent().expect("the area"))
            .expect("the area reads")
            .flatten()
            .map(|entry| entry.path())
            .filter(|entry| {
                entry
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(crate::state::QUARANTINE_PREFIX))
            })
            .collect();
        assert_eq!(leaves.len(), 1, "the quarantine leaf is kept");
        assert!(
            leaves[0].join("payload.bin").is_file(),
            "nothing was widened"
        );
        drop(root);
    }

    /// Replay's completion counters omit removals from a replay that stays
    /// pending. Both outcomes must flush before any measurement or new delete.
    #[test]
    fn completed_and_partial_replay_flush_before_measurement_or_new_deletion() {
        struct ReplayFlush<'a> {
            leaf: &'a std::path::Path,
            next_root: &'a std::path::Path,
            partial: bool,
            fail: bool,
            calls: Cell<usize>,
        }
        impl Flush for ReplayFlush<'_> {
            fn flush_volume(&self, _: &VerifiedVolume, _: Instant) -> crate::error::Result<()> {
                assert_eq!(self.calls.get(), 0, "replay flush is never retried");
                assert!(self.next_root.is_dir(), "flush must precede new deletion");
                assert_eq!(self.leaf.is_dir(), self.partial);
                assert!(
                    !self.leaf.join("payload.bin").exists(),
                    "replay already removed payload"
                );
                self.calls.set(1);
                if self.fail {
                    Err(crate::error::Error::new(
                        ErrorKind::UnknownState,
                        "replay flush failed",
                    ))
                } else {
                    Ok(())
                }
            }
        }
        struct AfterFlush<'a> {
            flush: &'a ReplayFlush<'a>,
            meter: Meter,
        }
        impl SpaceFacts for AfterFlush<'_> {
            fn measure_space(
                &self,
                volume: &VerifiedVolume,
            ) -> crate::error::Result<SpaceMeasurement> {
                assert_eq!(
                    self.flush.calls.get(),
                    1,
                    "no deciding measurement before replay flush"
                );
                assert!(!self.flush.fail, "failed replay flush forbids measurement");
                assert!(
                    self.flush.next_root.is_dir(),
                    "no new delete precedes the deciding measurement"
                );
                self.meter.measure_space(volume)
            }
        }
        for partial in [false, true] {
            for fail in [false, true] {
                let harness = Harness::new("space-prune-replay-flush", RESERVE, FLOOR);
                let (first, first_path) = harness.seed("pool", 1_000);
                let (_, next_path) = harness.seed("pool", 2_000);
                let (_, bytes) = harness.preview();
                let faults = Faults::new().with_device_shift(
                    "payload.bin",
                    crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
                );
                harness
                    .run_with(
                        &faults,
                        &bytes,
                        &Meter::new(&[4]),
                        &Flusher::ok(),
                        SpacePruneBounds::design(),
                    )
                    .expect_err("interrupt after capture, before unlink");
                let journal =
                    state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
                let leaf = first_path
                    .parent()
                    .expect("area")
                    .join(journal.records[0].capture_name());
                assert!(leaf.join("payload.bin").is_file());
                if partial {
                    state::fail_next_phase_publish(state::IntentPhase::MarkerLast);
                }
                let flush = ReplayFlush {
                    leaf: &leaf,
                    next_root: &next_path,
                    partial,
                    fail,
                    calls: Cell::new(0),
                };
                let space = AfterFlush {
                    flush: &flush,
                    meter: Meter::new(&[20]),
                };
                let digest = sha256_hex(&bytes);
                let request = harness.request(&digest, &bytes, SpacePruneBounds::design());
                let result = prune(
                    &harness.fixture.state,
                    &harness.fixture.platform(),
                    &harness.facts,
                    &harness.fixture.mounts(),
                    &harness.observer,
                    &space,
                    &flush,
                    &request,
                );
                assert_eq!(flush.calls.get(), 1);
                assert!(next_path.is_dir());
                assert_eq!(leaf.is_dir(), partial);
                assert!(!leaf.join("payload.bin").exists());
                if fail {
                    let error = result.expect_err("failed replay flush stops the pass");
                    assert_eq!(error.kind(), ErrorKind::UnknownState);
                    assert_eq!(error.kind().exit_code(), 3);
                    assert_eq!(space.meter.calls(), 0);
                    assert_eq!(error.detail()["replay"]["examined"], 1);
                    assert_eq!(error.detail()["replay"]["resumed"], usize::from(!partial));
                    assert_eq!(
                        error.detail()["replay"]["pending"]
                            .as_array()
                            .expect("pending")
                            .len(),
                        usize::from(partial)
                    );
                    assert_eq!(
                        error.detail()["replay"]["resumed_root_ids"],
                        if partial {
                            serde_json::json!([])
                        } else {
                            serde_json::json!([first])
                        }
                    );
                    assert_eq!(error.detail()["flush"]["attempts"], 1);
                    assert_eq!(error.detail()["flush"]["failures"], 1);
                } else {
                    let report = result.expect("flushed replay decides from fresh measurement");
                    let effect = report.effect.as_ref().expect("effect");
                    assert_eq!(space.meter.calls(), 1);
                    assert_eq!(
                        effect.replay.as_ref().expect("replay").resumed,
                        usize::from(!partial)
                    );
                    assert_eq!(
                        effect.replay.as_ref().expect("replay").pending,
                        usize::from(partial)
                    );
                    assert_eq!(effect.flush.attempts, 1);
                    assert_eq!(effect.pruned.count, 0, "replay alone satisfied the target");
                    assert_eq!(effect.batches, 0);
                    assert_eq!(effect.before.free_bytes, 20);
                    read_prune_report(&encode(&report)).expect("replay flush report reads back");
                }
            }
        }
    }

    #[test]
    fn maintain_space_disabled_preview_and_real() {
        let harness = Harness::new("maintain-space-disabled", RESERVE, FLOOR);
        let meter = Meter::new(&[1]);
        let flusher = Flusher::ok();
        let request = crate::prune::MaintainRequest {
            preview: true,
            plan_digest: None,
            plan_bytes: None,
            sweep_bounds: crate::scratch::SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            maintain_bounds: crate::prune::MaintainBounds::design(),
            now_ms: harness.now_ms,
        };
        let preview = crate::prune::maintain_with_space(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &meter,
            &flusher,
            &request,
            false,
        )
        .expect("preview");
        assert_eq!(preview.space_stage, "disabled");
        assert!(preview.space_triggered.is_none());
        let bytes = crate::prune::canonical_plan_bytes(&preview.plan).expect("plan");
        let real_request = crate::prune::MaintainRequest {
            preview: false,
            plan_digest: Some(&preview.plan_digest),
            plan_bytes: Some(&bytes),
            ..request
        };
        let real = crate::prune::maintain_with_space(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &meter,
            &flusher,
            &real_request,
            false,
        )
        .expect("real");
        assert_eq!(real.space_stage, "disabled");
        assert_eq!(meter.calls(), 0);
        assert_eq!(flusher.calls(), 0);
    }

    #[test]
    fn maintain_space_preview_and_trigger() {
        let harness = Harness::new("maintain-space-preview", RESERVE, FLOOR);
        let request = crate::prune::MaintainRequest {
            preview: true,
            plan_digest: None,
            plan_bytes: None,
            sweep_bounds: crate::scratch::SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            maintain_bounds: crate::prune::MaintainBounds::design(),
            now_ms: harness.now_ms,
        };
        let run = |enabled| {
            crate::prune::maintain_with_space(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[8]),
                &Flusher::ok(),
                &request,
                enabled,
            )
            .expect("preview")
        };
        let disabled = run(false);
        let enabled = run(true);
        assert_eq!(disabled.plan_digest, enabled.plan_digest);
        assert_eq!(
            crate::prune::canonical_plan_bytes(&disabled.plan).unwrap(),
            crate::prune::canonical_plan_bytes(&enabled.plan).unwrap()
        );
        assert_eq!(enabled.candidates, 0);
        assert_eq!(enabled.space_triggered, Some(1));
        assert!(disabled.space_triggered.is_none());
    }

    #[test]
    fn maintain_space_decision_table() {
        for (free, expected) in [
            (30, "admit"),
            (8, "admit_below_reserve"),
            (1, "space_below_floor"),
        ] {
            let harness = Harness::new("maintain-space-decision", RESERVE, FLOOR);
            let request = crate::prune::MaintainRequest {
                preview: true,
                plan_digest: None,
                plan_bytes: None,
                sweep_bounds: crate::scratch::SweepBounds::default(),
                delete_bounds: DeleteBounds::default(),
                maintain_bounds: crate::prune::MaintainBounds::design(),
                now_ms: harness.now_ms,
            };
            let preview = crate::prune::maintain_with_space(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[free]),
                &Flusher::ok(),
                &request,
                true,
            )
            .expect("preview");
            let bytes = crate::prune::canonical_plan_bytes(&preview.plan).unwrap();
            let real = crate::prune::MaintainRequest {
                preview: false,
                plan_digest: Some(&preview.plan_digest),
                plan_bytes: Some(&bytes),
                ..request
            };
            let report = crate::prune::maintain_with_space(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[free]),
                &Flusher::ok(),
                &real,
                true,
            )
            .expect("real");
            assert_eq!(report.space_rows.unwrap().rows[0]["decision"], expected);
        }
    }

    #[test]
    fn maintain_space_remaining_budget() {
        let harness = Harness::new("maintain-space-deadline", RESERVE, FLOOR);
        let meter = Meter::new(&[8]);
        let request = crate::prune::MaintainRequest {
            preview: true,
            plan_digest: None,
            plan_bytes: None,
            sweep_bounds: crate::scratch::SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            maintain_bounds: crate::prune::MaintainBounds {
                wall: Duration::ZERO,
                stop_margin: Duration::ZERO,
            },
            now_ms: harness.now_ms,
        };
        let report = crate::prune::maintain_with_space(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &meter,
            &Flusher::ok(),
            &request,
            true,
        )
        .expect("preview");
        assert!(report.timed_out);
        assert_eq!(meter.calls(), 0);
    }

    #[test]
    fn maintain_space_excludes_kept_and_undeclared() {
        for kept in [false, true] {
            let fixture = VolumeFixture::under_scratch("maintain-space-policy");
            let mut record = fixture.record.clone();
            if kept {
                record.durability = Durability::Kept;
                record.space = Some(SpacePolicy {
                    reserve_bytes: RESERVE,
                    floor_bytes: FLOOR,
                    declared_by: "fixture".to_owned(),
                    declared_ms: 1,
                });
            }
            fixture.register_record(&record);
            let meter = Meter::new(&[1]);
            let request = crate::prune::MaintainRequest {
                preview: true,
                plan_digest: None,
                plan_bytes: None,
                sweep_bounds: crate::scratch::SweepBounds::default(),
                delete_bounds: DeleteBounds::default(),
                maintain_bounds: crate::prune::MaintainBounds::design(),
                now_ms: state::now_ms(),
            };
            let report = crate::prune::maintain_with_space(
                &fixture.state,
                &fixture.platform(),
                &fixture.facts(),
                &fixture.mounts(),
                &fixture.observer(),
                &meter,
                &Flusher::ok(),
                &request,
                true,
            )
            .expect("preview");
            assert_eq!(report.space_triggered, Some(0));
            assert_eq!(meter.calls(), 0);
            assert_eq!(
                report.space_rows.unwrap().rows[0]["decision"],
                if kept {
                    "skipped_kept"
                } else {
                    "space_undeclared"
                }
            );
        }
    }

    #[test]
    fn future_host_job_space_trigger_runs_once() {
        let harness = Harness::new("future-host-space", RESERVE, FLOOR);
        let request = crate::prune::MaintainRequest {
            preview: true,
            plan_digest: None,
            plan_bytes: None,
            sweep_bounds: crate::scratch::SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            maintain_bounds: crate::prune::MaintainBounds::design(),
            now_ms: harness.now_ms,
        };
        let preview = crate::prune::maintain_with_space(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[8]),
            &Flusher::ok(),
            &request,
            true,
        )
        .expect("preview");
        let bytes = crate::prune::canonical_plan_bytes(&preview.plan).expect("plan");
        let mut calls = 0;
        if preview.candidates > 0 || preview.space_triggered.unwrap_or(0) > 0 {
            calls += 1;
            let real = crate::prune::MaintainRequest {
                preview: false,
                plan_digest: Some(&preview.plan_digest),
                plan_bytes: Some(&bytes),
                ..request
            };
            crate::prune::maintain_with_space(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[30]),
                &Flusher::ok(),
                &real,
                true,
            )
            .expect("real");
        }
        assert_eq!(calls, 1);
        let no_work = crate::prune::MaintainRequest {
            preview: true,
            plan_digest: None,
            plan_bytes: None,
            sweep_bounds: crate::scratch::SweepBounds::default(),
            delete_bounds: DeleteBounds::default(),
            maintain_bounds: crate::prune::MaintainBounds::design(),
            now_ms: harness.now_ms,
        };
        let preview = crate::prune::maintain_with_space(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[30]),
            &Flusher::ok(),
            &no_work,
            true,
        )
        .expect("preview without pressure");
        let mut no_work_calls = 0;
        if preview.candidates > 0 || preview.space_triggered.unwrap_or(0) > 0 {
            no_work_calls += 1;
        }
        assert_eq!(no_work_calls, 0);
    }

    #[test]
    fn guard_undeclared_area_never_enters_core() {
        let harness = Harness::new("guard-missing-area", RESERVE, FLOOR);
        let meter = Meter::new(&[8]);
        let result = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &meter,
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "missing",
                area: "missing",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        );
        assert_eq!(result.unwrap_err().kind(), ErrorKind::UnknownState);
        assert_eq!(meter.calls(), 0);
    }

    #[test]
    fn guard_decision_table_and_recheck_use_fresh_measurements() {
        for (reserve, floor, free) in [
            (10, 20, 10),
            (10, 20, 20),
            (10, 20, 21),
            (20, 20, 20),
            (20, 20, 21),
            (100, 20, 10),
            (100, 20, 20),
            (100, 20, 50),
            (100, 20, 100),
            (100, 20, 101),
        ] {
            let harness = Harness::new("guard-decision", reserve, floor);
            let meter = Meter::new(&[free, free]);
            let result = crate::space::guard(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &meter,
                &Flusher::ok(),
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
            );
            if free <= floor {
                assert_eq!(result.unwrap_err().kind(), ErrorKind::SpaceBelowFloor);
            } else {
                let report = result.expect("above floor admits");
                let expected = if free <= reserve {
                    crate::space::SpaceDecision::AdmitBelowReserve
                } else {
                    crate::space::SpaceDecision::Admit
                };
                assert_eq!(report.decision, expected);
                let token = crate::space::SpaceGuard::parse(&report.guard).expect("token");
                let again = crate::space::recheck(
                    &harness.fixture.state,
                    &harness.fixture.platform(),
                    &harness.facts,
                    &harness.fixture.mounts(),
                    &harness.observer,
                    &Meter::new(&[free, free]),
                    &Flusher::ok(),
                    &token,
                    Duration::from_secs(30),
                )
                .expect("recheck");
                assert_eq!(again.decision, crate::space::SpaceDecision::Continue);
            }
            assert!(meter.calls() <= 4);
        }
    }

    #[test]
    fn guard_prunes_and_stops_or_admits_from_flushed_measurement() {
        for (after, expect_stop) in [(10, true), (50, false)] {
            let harness = Harness::new("guard-prune", 100, 20);
            let (root, path) = harness.seed("pool", 1_000);
            let meter = Meter::new(&[10, 10, after]);
            let flush = Flusher::ok();
            let report = crate::space::guard(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &meter,
                &flush,
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
            )
            .expect("guard completed");
            assert!(!path.exists());
            assert_eq!(report.pruned.rows[0].root_id, root);
            assert_eq!(report.after.free_bytes, after);
            assert_eq!(
                report.decision,
                if expect_stop {
                    crate::space::SpaceDecision::StopBelowFloor
                } else {
                    crate::space::SpaceDecision::AdmitBelowReserve
                }
            );
            assert_eq!(flush.calls(), 1);
            assert!(meter.calls() <= 4);
        }
    }

    #[test]
    fn guard_flush_failure_never_admits_after_deletion() {
        let harness = Harness::new("guard-flush-failure", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::failing(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("flush failure stops admission");
        assert!(!path.exists());
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["pruned_root_ids"], serde_json::json!([root]));
    }

    #[test]
    fn guard_budget_counterfactuals_and_flush_override() {
        for (free, max_roots, fail_flush) in [
            (50, 0, false),
            (50, 1, false),
            (10, 0, false),
            (10, 1, false),
            (10, 1, true),
        ] {
            let harness = Harness::new("guard-budget", 100, 20);
            let (first, first_path) = harness.seed("pool", 1_000);
            let (_, second_path) = harness.seed("pool", 2_000);
            let meter = Meter::new(&[free, free, free]);
            let flush = if fail_flush {
                Flusher::failing()
            } else {
                Flusher::ok()
            };
            let mut bounds = SpacePruneBounds::design();
            bounds.roots = max_roots;
            let result = crate::space::guard::guard_with_bounds(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &meter,
                &flush,
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
                bounds,
            );
            if fail_flush {
                let error = result.expect_err("failed flush is unknown");
                assert_eq!(error.kind(), ErrorKind::UnknownState);
                assert_eq!(
                    error.detail()["pruned_root_ids"],
                    serde_json::json!([first])
                );
            } else if free <= 20 && max_roots == 0 {
                let error = result.expect_err("below floor, no deletion");
                assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
                assert_eq!(error.detail()["cause"], "budget_exhausted");
                assert_eq!(error.detail()["not_reached"]["count"], 2);
            } else {
                let report = result.expect("completed guard");
                assert_eq!(
                    report.decision,
                    if free <= 20 {
                        crate::space::SpaceDecision::StopBelowFloor
                    } else {
                        crate::space::SpaceDecision::AdmitBelowReserve
                    }
                );
                assert_eq!(report.pruned.count, max_roots);
                assert_eq!(report.not_reached.count, 2 - max_roots);
                assert_eq!(report.detail["cause"], "budget_exhausted");
            }
            assert_eq!(first_path.exists(), max_roots == 0);
            assert!(second_path.exists());
            assert!(meter.calls() <= 4);
        }
    }

    #[test]
    fn guard_skips_held_root_and_reports_no_effect_refusal() {
        let harness = Harness::new("guard-held", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        harness.hold(&root);
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("held root is not deleted and floor is breached");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert!(path.join("payload.bin").exists());
        assert_eq!(error.detail()["pruned_root_ids"], serde_json::json!([]));
        assert!(error.detail()["held_or_unknown_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0));
        assert_eq!(error.detail()["owned_prunable_bytes"], 0);
    }

    #[test]
    fn guard_decides_after_busy_volume_lock_without_deleting() {
        let harness = Harness::new("guard-wait", 100, 20);
        let (_, path) = harness.seed("pool", 1_000);
        let mut locks = LockSet::with_deadline(
            harness.fixture.state.clone(),
            Deadline::after(Duration::from_secs(1)),
        );
        let _lock = locks
            .lock_volume(&harness.record.volume_uuid)
            .expect("held volume lock");
        let meter = Meter::new(&[10, 50]);
        let report = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &meter,
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("fresh measurement above floor admits");
        assert_eq!(
            report.decision,
            crate::space::SpaceDecision::AdmitBelowReserve
        );
        assert!(report.decided_after_wait);
        assert_eq!(report.after.free_bytes, 50);
        assert!(path.join("payload.bin").exists());
        assert_eq!(meter.calls(), 2);
    }

    #[test]
    fn recheck_reverifies_token_volume_and_ignores_forged_observations() {
        let harness = Harness::new("guard-token-binding", 10, 5);
        let token = crate::space::SpaceGuard {
            volume_uuid: harness.record.volume_uuid.clone(),
            namespace: "example".to_owned(),
            area: "pool".to_owned(),
            generation: u64::MAX,
            observed_ms: u64::MAX,
        };
        let call = |facts: &FakeVolume, token: &crate::space::SpaceGuard| {
            crate::space::recheck(
                &harness.fixture.state,
                &harness.fixture.platform(),
                facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[30]),
                &Flusher::ok(),
                token,
                Duration::from_secs(30),
            )
        };
        let fresh = call(&harness.facts, &token).expect("forged observations grant nothing");
        assert_eq!(fresh.decision, crate::space::SpaceDecision::Continue);
        assert_ne!(
            crate::space::SpaceGuard::parse(&fresh.guard)
                .expect("token")
                .generation,
            u64::MAX
        );
        let mut older = token.clone();
        older.generation = 0;
        older.observed_ms = 0;
        assert_eq!(
            call(&harness.facts, &older).expect("old token").decision,
            fresh.decision
        );
        let mut wrong = harness.fixture.facts();
        wrong.uuid = VolumeUuid::parse(crate::testkit::OTHER_UUID).expect("other UUID");
        assert_eq!(
            call(&wrong, &token)
                .expect_err("different mounted UUID")
                .kind(),
            ErrorKind::VolumeMismatch
        );
        harness.facts.present.set(false);
        assert_eq!(
            call(&harness.facts, &token)
                .expect_err("absent volume")
                .kind(),
            ErrorKind::VolumeAbsent
        );
        harness.facts.present.set(true);
        let mut unregistered = token.clone();
        unregistered.volume_uuid = VolumeUuid::parse(crate::testkit::OTHER_UUID).expect("UUID");
        assert_eq!(
            call(&harness.facts, &unregistered)
                .expect_err("unregistered")
                .kind(),
            ErrorKind::UnknownState
        );
        let mut undeclared = token.clone();
        undeclared.area = "missing".to_owned();
        assert_eq!(
            call(&harness.facts, &undeclared)
                .expect_err("undeclared")
                .kind(),
            ErrorKind::UnknownState
        );
        let parent = harness.fixture.root.join("example");
        std::fs::create_dir_all(&parent).expect("fixture area parent");
        let target = harness.fixture.root.join("local-only-target");
        std::fs::create_dir(&target).expect("fixture target");
        std::os::unix::fs::symlink(&target, parent.join("pool")).expect("fixture symlink");
        assert!(
            call(&harness.facts, &token).is_err(),
            "an unresolvable area is refused"
        );
    }

    #[test]
    fn recheck_applies_current_floor_and_reserve() {
        let harness = Harness::new("guard-policy-edit", 10, 5);
        let token = crate::space::SpaceGuard {
            volume_uuid: harness.record.volume_uuid.clone(),
            namespace: "example".to_owned(),
            area: "pool".to_owned(),
            generation: harness.record.generation,
            observed_ms: 1,
        };
        let mut changed = harness.record.clone();
        changed.generation += 1;
        changed.space.as_mut().expect("policy").floor_bytes = 20;
        changed.space.as_mut().expect("policy").reserve_bytes = 100;
        harness.fixture.register_record(&changed);
        let error = crate::space::recheck(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &token,
            Duration::from_secs(30),
        )
        .expect_err("new floor stops work");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert_eq!(error.detail()["floor_bytes"], 20);
        assert_eq!(error.detail()["reserve_bytes"], 100);
    }

    #[test]
    fn guard_replans_a_root_released_during_the_previous_batch() {
        let harness = Harness::new("guard-release-mid-pass", 100, 20);
        let (_, first_path) = harness.seed("pool", 1_000);
        let (second, second_path) = harness.seed("pool", 2_000);
        harness.hold(&second);
        let state = harness.fixture.state.clone();
        let released = std::rc::Rc::new(Cell::new(false));
        let signalled = released.clone();
        let second_id = second.clone();
        let dirs = Faults::new();
        dirs.observing(Box::new(move |name| {
            if name == "payload.bin" && !first_path.exists() && !signalled.replace(true) {
                area::release(&state, &FakeObserver::new(), &second_id, "consumer")
                    .expect("release during first delete");
            }
        }));
        let meter = Meter::new(&[10, 10, 15, 30]);
        let mut bounds = SpacePruneBounds::design();
        bounds.batch_roots = 1;
        let report = crate::space::guard::guard_with_bounds(
            &harness.fixture.state,
            &dirs,
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &meter,
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
            bounds,
        )
        .expect("guard");
        assert!(released.get());
        assert!(
            !second_path.exists(),
            "the released root was considered in the next batch"
        );
        assert_eq!(report.pruned.count, 2);
        assert_eq!(meter.calls(), 4);
    }

    #[test]
    fn guard_pending_intent_keeps_only_covered_root_and_attributes_it() {
        let harness = Harness::new("guard-pending", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[10]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("leave pending intent");
        assert!(!path.exists());
        harness.hold(&root);
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("floor refuses, pending root stays kept");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert_eq!(
            error.detail()["pending_intents"][0]["reason"],
            "pending_intent"
        );
        assert_eq!(error.detail()["pending_intents"][0]["root_id"], root);
        assert!(error.detail()["held_or_unknown_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0));
    }

    #[test]
    fn guard_attributes_excluded_root_without_touching_it() {
        let harness = Harness::new("guard-excluded", 100, 20);
        harness.declare("archive", ReserveParticipation::Excluded);
        let (_, path) = harness.seed("archive", 1_000);
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("floor refuses");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert!(path.join("payload.bin").exists());
        assert!(error.detail()["excluded_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0));
    }

    #[test]
    fn guard_volume_disappears_after_capture_and_during_flush() {
        for during_flush in [false, true] {
            let harness = Harness::new("guard-volume-loss", 100, 20);
            let (_, path) = harness.seed("pool", 1_000);
            let facts = std::rc::Rc::new(harness.fixture.facts());
            let pull = facts.clone();
            let captured_path = path.clone();
            let dirs = std::rc::Rc::new(Faults::new());
            if !during_flush {
                let weak = std::rc::Rc::downgrade(&dirs);
                dirs.observing(Box::new(move |name| {
                    if name == "payload.bin" && !captured_path.exists() {
                        pull.present.set(false);
                        if let Some(dirs) = weak.upgrade() {
                            *dirs.observation_error.borrow_mut() = Some((
                                name.to_owned(),
                                std::io::Error::from_raw_os_error(libc::EIO),
                            ));
                        }
                    }
                }));
            }
            struct PullFlush<'a> {
                facts: &'a FakeVolume,
                pull: bool,
            }
            impl Flush for PullFlush<'_> {
                fn flush_volume(&self, _: &VerifiedVolume, _: Instant) -> crate::error::Result<()> {
                    if self.pull {
                        self.facts.present.set(false);
                    }
                    Ok(())
                }
            }
            let error = crate::space::guard(
                &harness.fixture.state,
                dirs.as_ref(),
                facts.as_ref(),
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[10, 10]),
                &PullFlush {
                    facts: facts.as_ref(),
                    pull: during_flush,
                },
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
            )
            .expect_err("volume loss stops guard");
            assert!(!facts.present.get(), "loss injection was reached");
            assert_eq!(error.kind(), ErrorKind::UnknownState);
            if during_flush {
                assert_eq!(
                    error.detail()["pruned_root_ids"]
                        .as_array()
                        .expect("ids")
                        .len(),
                    1
                );
            } else {
                let journal =
                    state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
                assert!(
                    !PendingIntent::pending(&journal.records).is_empty(),
                    "error={error:?}; phases={:?}; path={}",
                    journal.records.iter().map(|r| r.phase).collect::<Vec<_>>(),
                    path.exists()
                );
            }
        }
    }

    #[test]
    fn guard_foreign_open_child() {
        let Some(dir) = std::env::var_os("BARGE_FOREIGN_OPEN_FIXTURE") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let mut file = std::fs::File::open(dir.join("foreign-open")).expect("open foreign file");
        std::fs::write(dir.join("foreign-ready"), b"ready").expect("ready");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !dir.join("foreign-finish").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        use std::io::Read;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .expect("read held descriptor");
        assert_eq!(text, "open payload");
    }

    #[test]
    fn guard_reports_unlinked_but_open_foreign_file_and_zero_reclaim() {
        let harness = Harness::new("guard-open-foreign", 100, 20);
        let (_, path) = harness.seed("pool", 1_000);
        let foreign = harness.fixture.root.join("foreign-open");
        std::fs::write(&foreign, b"open payload").expect("foreign file");
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "space::effect::tests::guard_foreign_open_child"])
            .env("BARGE_FOREIGN_OPEN_FIXTURE", &harness.fixture.root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("foreign subprocess");
        let mut child = SignalChild(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !harness.fixture.root.join("foreign-ready").exists() && Instant::now() < deadline {
            if child.0.try_wait().expect("poll").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(harness.fixture.root.join("foreign-ready").exists());
        std::fs::remove_file(&foreign).expect("unlink while foreign descriptor is open");
        let report = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("completed stop");
        assert!(!path.exists());
        assert_eq!(report.decision, crate::space::SpaceDecision::StopBelowFloor);
        assert_eq!(report.detail["stop"], "space_not_reclaimed");
        assert_eq!(report.reclaimed_bytes, 0);
        std::fs::write(harness.fixture.root.join("foreign-finish"), b"done").expect("finish child");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().expect("poll") {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "foreign child exited within bound"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn guard_enospc_at_intent_write_precedes_effect() {
        let harness = Harness::new("guard-enospc", 100, 20);
        let (_, path) = harness.seed("pool", 1_000);
        let no_space = crate::os::test_no_space();
        let errno = no_space.raw_os_error().expect("ENOSPC");
        state::fail_next_intent_publish(no_space);
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("intent write fails");
        assert_eq!(error.detail()["errno"], errno);
        assert!(path.join("payload.bin").exists());
        harness.no_pending_intent();
    }

    #[test]
    fn guard_and_error_readers_refuse_each_others_schema() {
        let harness = Harness::new("guard-contracts", 10, 5);
        let report = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[30]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("guard");
        let bytes = crate::contract::success_bytes(
            crate::space::SPACE_GUARD_SCHEMA,
            "space guard",
            &report,
        )
        .expect("encode");
        assert_eq!(
            crate::space::read_guard_document(&bytes).expect("guard read"),
            report
        );
        assert!(crate::error::read_document(&bytes).is_err());
        let error = crate::error::Error::new(ErrorKind::SpaceBelowFloor, "below floor");
        let bytes = crate::contract::error_bytes("space guard", &error);
        assert_eq!(
            crate::error::read_document(&bytes)
                .expect("error read")
                .kind(),
            ErrorKind::SpaceBelowFloor
        );
        assert!(crate::space::read_guard_document(&bytes).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(
            &crate::contract::success_bytes(
                crate::space::SPACE_GUARD_SCHEMA,
                "space guard",
                &report,
            )
            .expect("encode"),
        )
        .expect("JSON");
        value["schema"] = "barge.space.guard/999".into();
        assert!(
            crate::space::read_guard_document(&serde_json::to_vec(&value).expect("encode"))
                .is_err()
        );
        value["schema"] = crate::space::SPACE_GUARD_SCHEMA.into();
        value["data"]["detail"]
            .as_object_mut()
            .expect("detail")
            .remove("replay_partial_root_ids");
        assert!(
            crate::space::read_guard_document(&serde_json::to_vec(&value).expect("encode"))
                .is_err()
        );
        value["data"]["detail"]["replay_partial_root_ids"] = "not an array".into();
        assert!(
            crate::space::read_guard_document(&serde_json::to_vec(&value).expect("encode"))
                .is_err()
        );
    }

    #[test]
    fn guard_uses_falling_post_flush_measurement() {
        let harness = Harness::new("guard-falling-free", 100, 20);
        let (_, path) = harness.seed("pool", 1_000);
        let report = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[50, 50, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("completed stop");
        assert!(!path.exists());
        assert_eq!(report.after.free_bytes, 10);
        assert_eq!(report.decision, crate::space::SpaceDecision::StopBelowFloor);
    }

    #[test]
    fn guard_refuses_overflow_and_undeclared_policy_and_durability() {
        struct Overflow;
        impl SpaceFacts for Overflow {
            fn measure_space(
                &self,
                volume: &VerifiedVolume,
            ) -> crate::error::Result<SpaceMeasurement> {
                SpaceMeasurement::from_blocks(
                    Some(u64::MAX),
                    Some(2),
                    Some(2),
                    Some(2),
                    1,
                    volume.identity().device,
                )
            }
        }
        let harness = Harness::new("guard-unknown", 100, 20);
        let run = |meter: &dyn SpaceFacts| {
            crate::space::guard(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                meter,
                &Flusher::ok(),
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
            )
        };
        assert_eq!(
            run(&Overflow).expect_err("overflow").kind(),
            ErrorKind::UnknownState
        );
        let mut record = harness.record.clone();
        record.space = None;
        harness.fixture.register_record(&record);
        assert_eq!(
            run(&Meter::new(&[50]))
                .expect_err("undeclared policy")
                .detail()["space_undeclared"],
            true
        );
        record.space = harness.record.space.clone();
        record.durability = crate::registry::Durability::Kept;
        harness.fixture.register_record(&record);
        assert_eq!(
            run(&Meter::new(&[50])).expect_err("kept").kind(),
            ErrorKind::RefusedBoundary
        );
        record.durability = crate::registry::Durability::Undeclared;
        harness.fixture.register_record(&record);
        assert_eq!(
            run(&Meter::new(&[50]))
                .expect_err("undeclared durability")
                .kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn guard_holds_root_lock_during_the_live_effect() {
        let harness = Harness::new("guard-root-lock", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        let locked = std::rc::Rc::new(Cell::new(false));
        let flag = locked.clone();
        let state = harness.fixture.state.clone();
        let root_id = root.clone();
        let captured = path.clone();
        let dirs = Faults::new();
        dirs.observing(Box::new(move |name| {
            if name == "payload.bin" && !captured.exists() && !flag.get() {
                let attempt = area::hold(
                    &state,
                    &FakeObserver::new(),
                    &HoldRequest {
                        root_id: root_id.clone(),
                        holder: "racer".to_owned(),
                        pid: std::process::id(),
                        scope: HoldScope::Write,
                        purpose: "race".to_owned(),
                    },
                );
                assert_eq!(
                    attempt.expect_err("root lock is held").kind(),
                    ErrorKind::Locked
                );
                flag.set(true);
            }
        }));
        let report = crate::space::guard(
            &harness.fixture.state,
            &dirs,
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10, 30]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("guard");
        assert!(locked.get());
        assert!(!path.exists());
        assert_eq!(report.pruned.rows[0].root_id, root);
    }

    #[test]
    fn guard_stops_after_registry_corruption_or_kept_durability() {
        for kept in [false, true] {
            let harness = Harness::new("guard-registry-change", 100, 20);
            let (first, first_path) = harness.seed("pool", 1_000);
            let (_, second_path) = harness.seed("pool", 2_000);
            struct ChangeRegistry {
                state: crate::state::StatePaths,
                record: VolumeRecord,
                kept: bool,
            }
            impl Flush for ChangeRegistry {
                fn flush_volume(&self, _: &VerifiedVolume, _: Instant) -> crate::error::Result<()> {
                    if self.kept {
                        let mut record = self.record.clone();
                        record.generation += 1;
                        record.durability = crate::registry::Durability::Kept;
                        let mut locks = LockSet::with_deadline(
                            self.state.clone(),
                            Deadline::after(Duration::from_secs(1)),
                        );
                        let lock = locks.lock_registry()?;
                        crate::registry::RegistryWriter::new(&self.state).publish(
                            &lock,
                            &crate::registry::RegistryDocument::new(vec![record]),
                        )?;
                    } else {
                        std::fs::write(self.state.registry_path(), b"invalid registry")
                            .expect("corrupt registry at flush");
                    }
                    Ok(())
                }
            }
            let mut bounds = SpacePruneBounds::design();
            bounds.batch_roots = 1;
            let error = crate::space::guard::guard_with_bounds(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[10, 10, 15]),
                &ChangeRegistry {
                    state: harness.fixture.state.clone(),
                    record: harness.record.clone(),
                    kept,
                },
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
                bounds,
            )
            .expect_err("registry change stops further pruning");
            assert_eq!(error.kind(), ErrorKind::UnknownState);
            assert_eq!(
                error.detail()["pruned_root_ids"],
                serde_json::json!([first])
            );
            assert!(!first_path.exists());
            assert!(second_path.exists());
        }
    }

    #[test]
    fn guard_replay_completed_root_below_floor_requires_completed_stop() {
        let harness = Harness::new("guard-replay-stop", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[10]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("leave pending intent");
        assert!(!path.exists());
        let result = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        );
        let report = result
            .expect("a replay that completed a root changed data and is not a no-effect refusal");
        assert_eq!(report.decision, crate::space::SpaceDecision::StopBelowFloor);
        assert!(report.detail["replayed_root_ids"]
            .as_array()
            .expect("ids")
            .iter()
            .any(|id| id == &root));
    }

    #[test]
    fn guard_replay_completed_and_partial_effects_above_and_below_floor() {
        for (partial, free) in [(false, 10), (false, 50), (true, 10), (true, 50)] {
            let harness = Harness::new("guard-replay-outcome", 100, 20);
            let (root, path) = harness.seed("pool", 1_000);
            let (_, bytes) = harness.preview();
            let faults = Faults::new().with_device_shift(
                "payload.bin",
                crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
            );
            harness
                .run_with(
                    &faults,
                    &bytes,
                    &Meter::new(&[10]),
                    &Flusher::ok(),
                    SpacePruneBounds::design(),
                )
                .expect_err("pending");
            if partial {
                state::fail_next_phase_publish(state::IntentPhase::MarkerLast);
            }
            let report = crate::space::guard(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &Meter::new(&[10, free]),
                &Flusher::ok(),
                &crate::space::GuardRequest {
                    volume_uuid: &harness.record.volume_uuid,
                    namespace: "example",
                    area: "pool",
                    wall: Duration::from_secs(30),
                    recheck: false,
                },
            )
            .expect("completed guard");
            assert_eq!(
                report.decision,
                if free <= 20 {
                    crate::space::SpaceDecision::StopBelowFloor
                } else {
                    crate::space::SpaceDecision::AdmitBelowReserve
                }
            );
            let key = if partial {
                "replay_partial_root_ids"
            } else {
                "replayed_root_ids"
            };
            assert_eq!(report.detail[key], serde_json::json!([root]));
            assert_eq!(report.detail["pruned_root_ids"], serde_json::json!([]));
            assert_eq!(report.detail["cause"], "replay");
            assert!(!path.exists());
            if partial {
                assert_eq!(report.kept.count, 1);
                assert_eq!(report.kept.rows[0].reason, "pending_intent");
                assert!(
                    report.detail["held_or_unknown_bytes"].as_u64().is_some(),
                    "detail: {}",
                    report.detail
                );
            }
        }
    }

    #[test]
    fn guard_journal_only_rollback_remains_a_no_effect_refusal() {
        let harness = Harness::new("guard-replay-settled", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[10]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("pending");
        let journal = state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
        let mut intent = journal.records[0].clone();
        let leaf = path.parent().expect("parent").join(intent.capture_name());
        std::fs::rename(&leaf, &path).expect("fixture restores original name");
        intent.phase = state::IntentPhase::Intent;
        intent.captures.clear();
        intent.foreign_captures.clear();
        std::fs::write(
            state::intent_path(&harness.fixture.state, &intent.intent_id).expect("intent path"),
            serde_json::to_vec(&intent).expect("intent bytes"),
        )
        .expect("fixture journal");
        struct HoldAfterReplay<'a> {
            state: &'a crate::state::StatePaths,
            root: &'a str,
        }
        impl Flush for HoldAfterReplay<'_> {
            fn flush_volume(&self, _: &VerifiedVolume, _: Instant) -> crate::error::Result<()> {
                area::hold(
                    self.state,
                    &FakeObserver::new(),
                    &HoldRequest {
                        root_id: self.root.to_owned(),
                        holder: "after-replay".to_owned(),
                        pid: std::process::id(),
                        scope: HoldScope::Write,
                        purpose: "fixture".to_owned(),
                    },
                )?;
                Ok(())
            }
        }
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &HoldAfterReplay {
                state: &harness.fixture.state,
                root: &root,
            },
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("journal-only settlement does not change refusal shape");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert_eq!(
            error.detail()["replay_settled_root_ids"],
            serde_json::json!([root])
        );
        assert!(path.join("payload.bin").exists());
    }

    #[test]
    fn guard_no_effect_refusal_can_create_state_lock_without_volume_effect() {
        let harness = Harness::new("guard-no-effect-lock", 100, 20);
        let lock_path = harness
            .fixture
            .state
            .volume_lock_path(&harness.record.volume_uuid);
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).expect("remove fixture lock");
        }
        assert!(!lock_path.exists());
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("below floor");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert!(
            lock_path.exists(),
            "the lock is state-only and creates no volume-byte effect"
        );
    }

    #[test]
    fn guard_keeps_root_when_holder_pid_was_reused() {
        let harness = Harness::new("guard-reused-pid", 100, 20);
        let (root, path) = harness.seed("pool", 1_000);
        let hold = area::hold(
            &harness.fixture.state,
            &harness.observer,
            &HoldRequest {
                root_id: root,
                holder: "reused".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "fixture".to_owned(),
            },
        )
        .expect("hold");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hold.path).expect("hold file")).expect("JSON");
        value["process_start"] = 1u64.into();
        std::fs::write(&hold.path, serde_json::to_vec(&value).expect("JSON"))
            .expect("fixture reused PID");
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("unknown holder keeps root");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        assert!(path.join("payload.bin").exists());
        assert!(error.detail()["held_or_unknown_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0));
    }

    #[test]
    fn guard_refuses_unreadable_intent_journal_before_effect() {
        let harness = Harness::new("guard-bad-journal", 100, 20);
        let (_, path) = harness.seed("pool", 1_000);
        let journal = harness.fixture.state.intents_dir();
        std::fs::remove_dir_all(&journal).expect("remove fixture journal");
        std::fs::write(&journal, b"not a directory").expect("unreadable journal fixture");
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("unknown journal");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(path.join("payload.bin").exists());
    }

    #[test]
    fn guard_low_space_never_reports_the_declared_fallback_path() {
        let harness = Harness::new("guard-no-fallback", 100, 20);
        let fallback = harness.fixture.dir.join("fallback");
        let mut declaration =
            Harness::declaration("pool", "example/pool", ReserveParticipation::Counts);
        declaration.absent = area::AbsentPolicy::FallbackLocal {
            path: fallback.clone(),
        };
        area::declare(&harness.fixture.state, &declaration, 2).expect("fallback declaration");
        let error = crate::space::guard(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[10, 10]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("low space refuses");
        assert_eq!(error.kind(), ErrorKind::SpaceBelowFloor);
        let serialized = crate::contract::error_bytes("space guard", &error);
        assert!(!String::from_utf8(serialized)
            .expect("UTF-8")
            .contains(&fallback.display().to_string()));
        assert!(!fallback.exists());
    }

    /// A device fault leaves a journal the next run can replay. The actual
    /// signal row has its own subprocess regression below.
    #[test]
    fn an_interrupted_prune_leaves_a_durable_intent_and_the_next_run_replays_it() {
        let harness = Harness::new("space-prune-replay", RESERVE, FLOOR);
        let (first, first_path) = harness.seed("pool", 1_000);
        let (second, second_path) = harness.seed("pool", 2_000);
        let _ = &first;
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("the fault stops the pass");
        assert!(
            !first_path.exists(),
            "the interrupted root is in its quarantine leaf"
        );
        drop(faults);
        // The injection is spent: the next run replays the intent from its recorded
        // identity and completes the deletion, then deletes the second root.
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the second run replays and prunes");
        let effect = report.effect.as_ref().expect("the effect");
        let replay = effect.replay.as_ref().expect("the replay is reported");
        assert_eq!(replay.examined, 1);
        assert_eq!(replay.pending, 0);
        assert!(replay.resumed + replay.settled == 1, "the intent settled");
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(
            effect.flush.attempts, 2,
            "replay and the new candidate batch each flush"
        );
        assert_eq!(
            effect.pruned.rows[0].root_id, second,
            "the replayed root settled through the journal, so the next planned root is this batch's deletion"
        );
        assert!(
            !first_path.exists(),
            "the replay completed the interrupted root"
        );
        assert!(!second_path.exists());
        harness.no_pending_intent();
    }

    #[test]
    fn an_unreadable_journal_of_a_pending_intent_stops_the_run_before_its_first_effect() {
        let harness = Harness::new("space-prune-pending", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("the first run leaves an intent pending");
        // The journal still holds the pending intent, and this run cannot replay it
        // because the journal is unreadable: the pass stops before its first effect.
        let journal = harness.fixture.state.intents_dir();
        std::fs::remove_dir_all(&journal).expect("the journal is removed");
        std::fs::write(&journal, b"not a directory").expect("a file takes its name");
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("an unreadable journal stops the run");
        assert_eq!(
            error.kind().exit_code(),
            3,
            "an unreadable journal is unknown state"
        );
        assert!(path.parent().expect("the area").is_dir());
    }

    #[test]
    fn a_device_change_injected_before_the_first_unlink_stops_the_delete() {
        let harness = Harness::new("space-prune-device", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        let error = harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("a device change before the first unlink stops the delete");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(
            !path.exists(),
            "the unit is in its quarantine leaf, not deleted"
        );
        let leaf = std::fs::read_dir(path.parent().expect("the area"))
            .expect("the area reads")
            .flatten()
            .map(|entry| entry.path())
            .find(|entry| {
                entry
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(crate::state::QUARANTINE_PREFIX))
            })
            .expect("the quarantine leaf is kept");
        assert!(leaf.join("payload.bin").is_file(), "the payload survives");
    }

    #[test]
    fn a_pulled_volume_is_refused_before_the_run_changes_anything() {
        let harness = Harness::new("space-prune-absent", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        harness.facts.present.set(false);
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("an absent volume refuses the run");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
        assert!(path.is_dir(), "nothing was measured, pruned or created");
        harness.no_pending_intent();
    }

    #[test]
    fn an_unwritable_state_volume_refuses_before_the_effect() {
        use std::os::unix::fs::PermissionsExt;

        let harness = Harness::new("space-prune-state-volume", RESERVE, FLOOR);
        let (_root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        // Permission refusal is separate from the ENOSPC injection below.
        let intents = harness.fixture.state.intents_dir();
        std::fs::set_permissions(&intents, std::fs::Permissions::from_mode(0o500))
            .expect("the journal directory is made read-only");
        let result = harness.run(
            &bytes,
            &Meter::new(&[4]),
            &Flusher::ok(),
            SpacePruneBounds::design(),
        );
        // Restore the mode first, so the fixture can clean itself up.
        std::fs::set_permissions(&intents, std::fs::Permissions::from_mode(0o700))
            .expect("the journal directory is writable again");
        let error = result.expect_err("an unwritable state volume refuses before the effect");
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(path.is_dir(), "nothing was deleted");
        harness.no_pending_intent();
    }

    /// Entry point of the real signalled subprocess; inert in an ordinary run.
    #[test]
    fn signal_prune_child() {
        let Some(base) = std::env::var_os("BARGE_SIGNAL_FIXTURE") else {
            return;
        };
        let base = std::path::PathBuf::from(base);
        crate::os::test_allow_termination().expect("known SIGTERM disposition in child");
        let harness = Harness::new("space-prune-signal-child", RESERVE, FLOOR);
        let (_, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        std::fs::write(base.join("info.json"), serde_json::to_vec(&serde_json::json!({
            "state": harness.fixture.state.root(), "mount": harness.fixture.root,
            "uuid": harness.fixture.uuid.as_str(), "device": harness.fixture.device.get(), "root": path,
        })).expect("info")).expect("publish owned fixture info");
        let dirs = Faults::new();
        dirs.observing(Box::new(move |name| {
            if name == "payload.bin" && !path.exists() {
                std::fs::write(base.join("ready"), b"captured").expect("signal handshake");
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }));
        harness
            .run_with(
                &dirs,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("parent signals at the captured-leaf barrier");
        panic!("signal barrier was not reached");
    }

    #[test]
    fn signal_guard_child() {
        let Some(base) = std::env::var_os("BARGE_GUARD_SIGNAL_FIXTURE") else {
            return;
        };
        let base = std::path::PathBuf::from(base);
        crate::os::test_allow_termination().expect("SIGTERM disposition");
        let harness = Harness::new("guard-signal-child", RESERVE, FLOOR);
        let (root_id, path) = harness.seed("pool", 1_000);
        std::fs::write(base.join("info.json"), serde_json::to_vec(&serde_json::json!({
            "state": harness.fixture.state.root(), "mount": harness.fixture.root,
            "uuid": harness.fixture.uuid.as_str(), "device": harness.fixture.device.get(), "root": path, "root_id": root_id,
        })).expect("info")).expect("publish fixture info");
        let dirs = Faults::new();
        dirs.observing(Box::new(move |name| {
            if name == "payload.bin" && !path.exists() {
                std::fs::write(base.join("ready"), b"captured").expect("handshake");
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }));
        crate::space::guard(
            &harness.fixture.state,
            &dirs,
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[4, 4]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &harness.record.volume_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("parent signals post-quarantine");
        panic!("signal barrier was not reached");
    }

    /// Ensure a failing subprocess assertion cannot leave a sleeping child.
    struct SignalChild(std::process::Child);
    impl Drop for SignalChild {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let until = Instant::now() + Duration::from_secs(1);
                while Instant::now() < until {
                    if self.0.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    #[test]
    fn guard_sigterm_releases_locks_and_next_guard_flushes_replay_before_measure() {
        let scratch = crate::testkit::TempDir::under_scratch("guard-sigterm");
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "space::effect::tests::signal_guard_child",
                "--nocapture",
            ])
            .env("BARGE_GUARD_SIGNAL_FIXTURE", scratch.path())
            .env(crate::testkit::SCRATCH_ROOT_ENV, scratch.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("guard subprocess");
        let mut child = SignalChild(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !scratch.join("ready").exists() && Instant::now() < deadline {
            if child.0.try_wait().expect("poll").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            scratch.join("ready").exists(),
            "post-quarantine barrier reached"
        );
        // A second guard actually contends with the paused guard's volume lock.
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.join("info.json")).expect("info"))
                .expect("JSON");
        let platform = crate::os::platform();
        let paths = crate::state::StatePaths::resolve(
            &platform,
            Some(std::path::Path::new(info["state"].as_str().expect("state"))),
        )
        .expect("state");
        let uuid = VolumeUuid::parse(info["uuid"].as_str().expect("uuid")).expect("UUID");
        let facts = FakeVolume {
            root: info["mount"].as_str().expect("mount").into(),
            uuid: uuid.clone(),
            device: crate::os::DeviceId::new(info["device"].as_u64().expect("device")),
            present: Cell::new(true),
            shifted: Cell::new(false),
            duplicate: Cell::new(false),
        };
        let contender_meter = Meter::new(&[4, 20]);
        let contender = crate::space::guard(
            &paths,
            &platform,
            &facts,
            &facts,
            &FakeObserver::new(),
            &contender_meter,
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("contending guard decides");
        assert!(contender.decided_after_wait);
        assert_eq!(contender.decision, crate::space::SpaceDecision::Admit);
        assert_eq!(contender.pruned.count, 0);
        assert_eq!(contender_meter.calls(), 2);
        // While the first guard remains paused, a guard on another registered
        // volume must be able to acquire its distinct lock and decide.
        let other = VolumeFixture::under_scratch("guard-other-volume");
        let other_uuid = VolumeUuid::parse(crate::testkit::OTHER_UUID).expect("other UUID");
        let mut other_record = other.record.clone();
        other_record.volume_uuid = other_uuid.clone();
        other_record.space = Some(SpacePolicy {
            reserve_bytes: RESERVE,
            floor_bytes: FLOOR,
            declared_by: "fixture".to_owned(),
            declared_ms: 1,
        });
        let original = registry::read(&paths)
            .expect("registry")
            .volume(&uuid)
            .expect("first record")
            .clone();
        let mut registry_locks =
            LockSet::with_deadline(paths.clone(), Deadline::after(Duration::from_secs(1)));
        let registry_lock = registry_locks.lock_registry().expect("registry lock");
        crate::registry::RegistryWriter::new(&paths)
            .publish(
                &registry_lock,
                &crate::registry::RegistryDocument::new(vec![original, other_record]),
            )
            .expect("two volumes");
        drop(registry_lock);
        drop(registry_locks);
        let other_facts = FakeVolume {
            root: other.root.clone(),
            uuid: other_uuid.clone(),
            device: other.device,
            present: Cell::new(true),
            shifted: Cell::new(false),
            duplicate: Cell::new(false),
        };
        let other_result = crate::space::guard(
            &paths,
            &platform,
            &other_facts,
            &other.mounts(),
            &FakeObserver::new(),
            &Meter::new(&[4, 4]),
            &Flusher::ok(),
            &crate::space::GuardRequest {
                volume_uuid: &other_uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect_err("other volume below floor");
        assert_eq!(other_result.kind(), ErrorKind::SpaceBelowFloor);
        assert_eq!(other_result.detail()["decided_after_wait"], false);
        crate::os::test_terminate(&mut child.0).expect("SIGTERM");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll") {
                break status;
            }
            assert!(Instant::now() < deadline, "child exits within bound");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(crate::os::test_was_terminated(status));
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.join("info.json")).expect("info"))
                .expect("JSON");
        let platform = crate::os::platform();
        let paths = crate::state::StatePaths::resolve(
            &platform,
            Some(std::path::Path::new(info["state"].as_str().expect("state"))),
        )
        .expect("state");
        let uuid = VolumeUuid::parse(info["uuid"].as_str().expect("uuid")).expect("UUID");
        let facts = FakeVolume {
            root: info["mount"].as_str().expect("mount").into(),
            uuid: uuid.clone(),
            device: crate::os::DeviceId::new(info["device"].as_u64().expect("device")),
            present: Cell::new(true),
            shifted: Cell::new(false),
            duplicate: Cell::new(false),
        };
        let journal = state::read_intents(&paths, INTENT_ROWS).expect("journal");
        assert_eq!(PendingIntent::pending(&journal.records).len(), 1);
        assert_eq!(journal.records[0].phase, state::IntentPhase::Renamed);
        let mut locks = LockSet::with_deadline(paths.clone(), Deadline::after(Duration::ZERO));
        let volume_lock = locks
            .lock_volume(&uuid)
            .expect("volume lock released immediately");
        let root_lock = locks
            .lock_root(info["root_id"].as_str().expect("root id"))
            .expect("root lock released immediately");
        drop(root_lock);
        drop(volume_lock);
        drop(locks);
        struct OrderedFlush<'a>(&'a Meter);
        impl Flush for OrderedFlush<'_> {
            fn flush_volume(&self, _: &VerifiedVolume, _: Instant) -> crate::error::Result<()> {
                assert_eq!(
                    self.0.calls(),
                    1,
                    "replay flush precedes authoritative measurement"
                );
                Ok(())
            }
        }
        let meter = Meter::new(&[4, 20]);
        let report = crate::space::guard(
            &paths,
            &platform,
            &facts,
            &facts,
            &FakeObserver::new(),
            &meter,
            &OrderedFlush(&meter),
            &crate::space::GuardRequest {
                volume_uuid: &uuid,
                namespace: "example",
                area: "pool",
                wall: Duration::from_secs(30),
                recheck: false,
            },
        )
        .expect("guard replays and decides");
        assert_eq!(report.decision, crate::space::SpaceDecision::Admit);
        assert_eq!(meter.calls(), 2);
        assert!(PendingIntent::pending(
            &state::read_intents(&paths, INTENT_ROWS)
                .expect("journal")
                .records
        )
        .is_empty());
    }

    #[test]
    fn sigterm_after_quarantine_leaves_a_replayable_durable_intent() {
        let scratch = crate::testkit::TempDir::under_scratch("space-prune-sigterm");
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "space::effect::tests::signal_prune_child",
                "--nocapture",
            ])
            .env("BARGE_SIGNAL_FIXTURE", scratch.path())
            .env(crate::testkit::SCRATCH_ROOT_ENV, scratch.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("owned prune subprocess");
        let mut child = SignalChild(child);
        let until = Instant::now() + Duration::from_secs(15);
        while !scratch.join("ready").exists() && Instant::now() < until {
            if child.0.try_wait().expect("poll").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let ready = scratch.join("ready").exists();
        if ready {
            crate::os::test_terminate(&mut child.0).expect("send actual SIGTERM");
        } else {
            let _ = child.0.kill();
        }
        let until = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll signalled child") {
                break status;
            }
            if Instant::now() >= until {
                let _ = child.0.kill();
                panic!("owned signalled child did not exit in bound");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(ready, "child reached post-quarantine barrier");
        assert!(
            crate::os::test_was_terminated(status),
            "SIGTERM is the observed exit cause"
        );
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.join("info.json")).expect("info"))
                .expect("decode info");
        let state_path = std::path::Path::new(info["state"].as_str().expect("state"));
        let platform = crate::os::platform();
        let paths =
            crate::state::StatePaths::resolve(&platform, Some(state_path)).expect("owned state");
        let facts = FakeVolume {
            root: info["mount"].as_str().expect("mount").into(),
            uuid: VolumeUuid::parse(info["uuid"].as_str().expect("uuid")).expect("UUID"),
            device: crate::os::DeviceId::new(info["device"].as_u64().expect("device")),
            present: Cell::new(true),
            shifted: Cell::new(false),
            duplicate: Cell::new(false),
        };
        let journal = state::read_intents(&paths, INTENT_ROWS).expect("durable journal");
        assert_eq!(PendingIntent::pending(&journal.records).len(), 1);
        assert_eq!(journal.records[0].phase, state::IntentPhase::Renamed);
        let root = std::path::Path::new(info["root"].as_str().expect("root"));
        assert!(!root.exists(), "quarantine preceded the signal");
        let leaf = root
            .parent()
            .expect("parent")
            .join(journal.records[0].capture_name());
        assert!(
            leaf.join("payload.bin").is_file(),
            "signal leaves captured payload intact"
        );
        let replay = crate::delete::replay_pending(
            &paths,
            &platform,
            &facts,
            &FakeObserver::new(),
            DeleteBounds::default(),
            None,
        )
        .expect("replay");
        assert_eq!(replay.resumed, 1);
        assert!(replay.pending.is_empty());
        assert!(!leaf.exists());
        assert!(PendingIntent::pending(
            &state::read_intents(&paths, INTENT_ROWS)
                .expect("journal")
                .records
        )
        .is_empty());
    }

    #[test]
    fn enospc_at_intent_publish_refuses_before_the_first_effect() {
        let harness = Harness::new("space-prune-enospc", RESERVE, FLOOR);
        let (_, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let no_space = crate::os::test_no_space();
        let errno = no_space.raw_os_error().expect("ENOSPC errno");
        state::fail_next_intent_publish(no_space);
        let error = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("ENOSPC prevents the intent and effect");
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(error.kind().exit_code(), 3);
        assert_eq!(error.detail()["errno"], errno);
        assert!(error.detail()["path"]
            .as_str()
            .expect("state volume path")
            .starts_with(harness.fixture.state.root().to_str().expect("UTF8")));
        assert!(path.join("payload.bin").is_file());
        harness.no_pending_intent();
        harness
            .run(
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("later healthy pass");
        assert!(!path.exists());
    }

    #[test]
    fn a_volume_pulled_after_quarantine_keeps_the_intent_until_reverified() {
        let harness = Harness::new("space-prune-mid-pull", RESERVE, FLOOR);
        let (_, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let facts = std::rc::Rc::new(harness.fixture.facts());
        let pulled = facts.clone();
        let dirs = Faults::new();
        *dirs.observation_error.borrow_mut() =
            Some(("payload.bin".to_owned(), crate::os::test_io_error()));
        dirs.observing(Box::new(move |name| {
            if name == "payload.bin" {
                pulled.present.set(false);
            }
        }));
        let digest = sha256_hex(&bytes);
        let request = harness.request(&digest, &bytes, SpacePruneBounds::design());
        let error = prune(
            &harness.fixture.state,
            &dirs,
            facts.as_ref(),
            &harness.fixture.mounts(),
            &harness.observer,
            &Meter::new(&[4]),
            &Flusher::ok(),
            &request,
        )
        .expect_err("mid-prune volume loss");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(
            error
                .to_string()
                .contains(&crate::os::test_io_error().to_string()),
            "actual EIO is the interrupted walk's failure: {error}"
        );
        assert_eq!(error.kind().exit_code(), 3);
        assert!(!path.exists(), "quarantine happened before the pull");
        let journal = state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
        assert_eq!(PendingIntent::pending(&journal.records).len(), 1);
        let leaf = path
            .parent()
            .expect("parent")
            .join(journal.records[0].capture_name());
        assert!(leaf.join("payload.bin").is_file());
        let replay = crate::delete::replay_pending(
            &harness.fixture.state,
            &harness.fixture.platform(),
            facts.as_ref(),
            &harness.observer,
            DeleteBounds::default(),
            None,
        )
        .expect("absent replay reports");
        assert_eq!(replay.pending.len(), 1);
        assert!(leaf.join("payload.bin").is_file());
        facts.present.set(true);
        harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("reverified volume replays");
        assert!(!leaf.exists());
        harness.no_pending_intent();
    }

    #[test]
    fn a_stalled_flush_is_killed_and_reaped_before_any_further_deletion() {
        struct Helper {
            mode: u8,
            elapsed: Cell<Duration>,
        }
        impl Flush for Helper {
            fn flush_volume(
                &self,
                _: &VerifiedVolume,
                deadline: Instant,
            ) -> crate::error::Result<()> {
                if self.mode == 4 {
                    assert!(
                        deadline.saturating_duration_since(Instant::now())
                            <= Duration::from_secs(59),
                        "flush budget is clipped to the pass, reserving reap time"
                    );
                    return Err(crate::error::Error::new(
                        ErrorKind::UnknownState,
                        "clipped-deadline fixture",
                    ));
                }
                let began = Instant::now();
                let result = crate::os::test_flush_helper(deadline, self.mode);
                self.elapsed.set(began.elapsed());
                result
            }
        }
        for mode in [0, 1, 2, 3, 4] {
            let harness = Harness::new("space-prune-flush-bound", RESERVE, FLOOR);
            let (first, first_path) = harness.seed("pool", 1_000);
            let (_, second_path) = harness.seed("pool", 2_000);
            let (_, bytes) = harness.preview();
            let digest = sha256_hex(&bytes);
            let bounds = SpacePruneBounds {
                batch_roots: 1,
                wall: Duration::from_secs(60),
                flush_deadline: if mode == 3 {
                    Duration::ZERO
                } else if mode == 4 {
                    Duration::from_secs(90)
                } else {
                    Duration::from_millis(100)
                },
                ..SpacePruneBounds::design()
            };
            let request = harness.request(&digest, &bytes, bounds);
            let meter = Meter::new(&[4]);
            let helper = Helper {
                mode,
                elapsed: Cell::new(Duration::ZERO),
            };
            let error = prune(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &harness.fixture.mounts(),
                &harness.observer,
                &meter,
                &helper,
                &request,
            )
            .expect_err("a stalled or failed flush stops the pass");
            assert!(
                helper.elapsed.get() < Duration::from_secs(5),
                "stalled helper must not block the pass"
            );
            assert_eq!(error.kind(), ErrorKind::UnknownState);
            assert_eq!(error.kind().exit_code(), 3);
            if mode < 3 {
                assert_eq!(error.detail()["flush_child_reaped"], true);
                assert_eq!(error.detail()["custody_uncertain"], false);
                if mode < 2 {
                    assert!(error.detail()["flush_child_start"].is_u64());
                }
                if mode < 2 {
                    assert_eq!(error.detail()["flush_child_killed"], true);
                    assert_eq!(error.detail()["flush_timed_out"], true);
                }
            }
            assert_eq!(
                error.detail()["pruned_root_ids"],
                serde_json::json!([first])
            );
            assert!(!first_path.exists());
            assert!(second_path.is_dir(), "no second delete after failed flush");
            assert_eq!(
                meter.calls(),
                1,
                "a failed flush never supplies a measurement"
            );
        }
    }

    /// An adopted root's directory name is not its root id, so a consumer's `hold`
    /// and the deletion take two different files unless the unit carries the root
    /// id. This is the counterfactual both halves of that rule:
    ///
    /// - a hold recorded before the pass keeps the root (`held`), and
    /// - a `hold` attempted while the pass holds the root's lock fails `locked`,
    ///   which can only be true if the deletion took the **root-id** lock the hold
    ///   takes rather than the unit key its directory name derives.
    #[test]
    fn an_adopted_roots_deletion_holds_the_root_id_lock_a_consumer_hold_takes() {
        let harness = Harness::new("space-prune-adopted", RESERVE, FLOOR);
        let (root_id, path, nonce) = harness.seed_adopted("pool", "adopted-run", 1_000);
        assert_ne!(root_id, "adopted-run", "the name is not the root id");
        let _ = nonce;
        let _ = &path;
        let (preview, bytes) = harness.preview();
        assert_eq!(preview.candidates, 1);
        assert_eq!(preview.plan.candidates.rows[0].root_id, root_id);
        assert_eq!(preview.plan.candidates.rows[0].name, "adopted-run");

        // A `hold` attempted during the effect waits on the lock the deletion holds
        // and then fails `locked`; without the root-id lock it would succeed.
        let refused = std::rc::Rc::new(RefCell::new(None));
        let refused_in_probe = std::rc::Rc::clone(&refused);
        let state = harness.fixture.state.clone();
        let probed = root_id.clone();
        let faults = Faults::new();
        faults.observing(Box::new(move |_name| {
            if refused_in_probe.borrow().is_some() {
                return;
            }
            let mut locks =
                LockSet::with_deadline(state.clone(), Deadline::after(Duration::from_millis(50)));
            if let Err(error) = locks.lock_root(&probed) {
                *refused_in_probe.borrow_mut() = Some(error.kind());
            }
        }));
        let report = harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        assert_eq!(report.effect.as_ref().expect("the effect").pruned.count, 1);
        assert_eq!(
            *refused.borrow(),
            Some(ErrorKind::Locked),
            "the deletion holds the root-id lock a hold takes"
        );

        // A hold recorded before the pass is read under that lock and keeps the root.
        let harness = Harness::new("space-prune-adopted-held", RESERVE, FLOOR);
        let (root_id, path, _nonce) = harness.seed_adopted("pool", "adopted-run", 1_000);
        let (_, bytes) = harness.preview();
        harness.hold(&root_id);
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        assert_eq!(
            held_reason(&report, &root_id).as_deref(),
            Some("held"),
            "a held adopted root is skipped and reported"
        );
        assert!(path.is_dir(), "the held adopted root is never touched");
    }

    /// The durable identity record must describe **this** candidate, not merely
    /// verify somewhere: a record whose path is another folder is a copy's record and
    /// keeps the root.
    #[test]
    fn a_copied_identity_record_does_not_authorize_this_root() {
        let harness = Harness::new("space-prune-copied-identity", RESERVE, FLOOR);
        let (root_id, path, nonce) = harness.seed_adopted("pool", "adopted-run", 1_000);
        let (preview, bytes) = harness.preview();
        let row = &preview.plan.candidates.rows[0];
        assert_eq!(row.root_id, root_id);
        // The record itself verifies: its inode, marker identity and volume are this
        // root's, and only its recorded path is another folder's.
        harness.write_identity(
            &root_id,
            row.inode,
            &nonce,
            "example/pool/another-adopted-run",
            "pool",
        );
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the pass runs");
        assert_eq!(
            held_reason(&report, &root_id).as_deref(),
            Some("identity_changed"),
            "a record for another path does not authorize this root"
        );
        let detail = report
            .effect
            .as_ref()
            .expect("the effect")
            .held
            .rows
            .iter()
            .find(|row| row.root_id == root_id)
            .and_then(|row| row.detail.clone())
            .expect("the refusal reads its evidence");
        assert!(
            detail.contains("does not describe this observation"),
            "the refusal names the binding: {detail}"
        );
        assert!(path.is_dir(), "the root is kept");
    }

    /// The journal is one account journal, but a pass is one volume's. Another
    /// volume's pending intent is neither replayed by this pass nor allowed to
    /// block it.
    #[test]
    fn another_volumes_pending_intent_neither_blocks_nor_is_replayed() {
        let harness = Harness::new("space-prune-two-volumes", RESERVE, FLOOR);
        let (root_a, path_a) = harness.seed("pool", 1_000);
        let (_, bytes_a) = harness.preview();

        // A second volume in the same account: its own tree, its own record, one
        // declaration and one idle root.
        let other = VolumeFixture::under_scratch("space-prune-two-volumes-b");
        let other_uuid = VolumeUuid::parse(crate::testkit::OTHER_UUID).expect("the second uuid");
        let mut record_b = other.record.clone();
        record_b.volume_uuid = other_uuid.clone();
        record_b.space = Some(SpacePolicy {
            reserve_bytes: RESERVE,
            floor_bytes: FLOOR,
            declared_by: "fixture".to_owned(),
            declared_ms: 1,
        });
        harness.register_all(&[harness.record.clone(), record_b.clone()]);
        area::declare(
            &harness.fixture.state,
            &Harness::declaration("other", "example/other", ReserveParticipation::Counts),
            1,
        )
        .expect("the second area declares");
        let path_b = other.root.join("example/other/adopted-run");
        std::fs::create_dir_all(&path_b).expect("the second root's directory is made");
        VolumeFixture::payload(&path_b, "payload.bin", &[3u8; 2048]);
        let platform = other.platform();
        let dirs: &dyn crate::os::DirOps = &platform;
        let other_observer = other.observer();
        let process: &dyn crate::os::ProcessObserver = &other_observer;
        let uid = process.effective_uid().expect("the uid");
        let marker = area::write_marker(
            &platform,
            &dirs.open_path(&path_b).expect("the second root opens"),
            &area::MarkerWrite {
                root_id: None,
                volume_uuid: &other_uuid,
                resolved_path: &path_b,
                mount_point: &other.root,
                area: area::MarkerArea::identity("example", "other"),
                kind: RootKind::AreaRoot,
                origin: area::RootOrigin::AdoptedExisting,
            },
            uid,
        )
        .expect("the second root's marker is written");
        {
            let record = serde_json::json!({
                "schema": "barge.activity/1",
                "root_id": marker.root_id,
                "volume_uuid": other_uuid.as_str(),
                "last_release_ms": 1_000,
                "first_created_ms": 900,
                "releases": 1,
                "generation": 1,
                "recorded_ms": 1_000,
            });
            let activity = harness
                .fixture
                .state
                .activity_path(&marker.root_id)
                .expect("the record path");
            std::fs::create_dir_all(activity.parent().expect("the activity directory"))
                .expect("the activity directory is made");
            std::fs::write(
                &activity,
                serde_json::to_vec_pretty(&record).expect("the record encodes"),
            )
            .expect("the record is written");
            std::fs::set_permissions(
                &activity,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            )
            .expect("the record is private");
        }
        let facts_b = crate::testkit::FakeVolume {
            root: other.root.clone(),
            uuid: other_uuid.clone(),
            device: other.device,
            present: std::cell::Cell::new(true),
            shifted: std::cell::Cell::new(false),
            duplicate: std::cell::Cell::new(false),
        };
        let observer = harness.fixture.observer();
        let (preview_b, bytes_b) = {
            let meter = Meter::new(&[4]);
            let report = plan(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &facts_b,
                &other.mounts(),
                &observer,
                &meter,
                &SpacePruneRequest {
                    volume_uuid: &other_uuid,
                    now_ms: harness.now_ms,
                    wall: SPACE_PLAN_WALL,
                    lock_wait: Duration::from_secs(1),
                },
            )
            .expect("the second volume's plan is built");
            let bytes = canonical_plan_bytes(&report.plan).expect("the plan encodes");
            (report, bytes)
        };
        assert_eq!(preview_b.candidates, 1);
        // Interrupt the second volume's pass so it leaves a pending intent, in this
        // account's one journal.
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(other.device.get().saturating_add(1)),
        );
        let digest = sha256_hex(&bytes_b);
        let request_b = SpacePruneRunRequest {
            volume_uuid: &other_uuid,
            plan_digest: &digest,
            plan_bytes: &bytes_b,
            now_ms: harness.now_ms,
            lock_wait: Duration::from_secs(1),
            delete_bounds: DeleteBounds {
                retry_pause: Duration::ZERO,
                ..DeleteBounds::default()
            },
            bounds: SpacePruneBounds::design(),
        };
        prune(
            &harness.fixture.state,
            &faults,
            &facts_b,
            &other.mounts(),
            &observer,
            &Meter::new(&[4]),
            &Flusher::ok(),
            &request_b,
        )
        .expect_err("the second volume's pass is interrupted");
        let journal =
            state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("the journal");
        assert_eq!(
            PendingIntent::pending(&journal.records)
                .iter()
                .filter(|intent| intent.volume_uuid == other_uuid)
                .count(),
            1,
            "the second volume left an intent pending"
        );

        // A requested-volume lock refusal must not replay another volume's
        // intent either. Compare the entire journal before allowing the pass.
        {
            let mut locks = LockSet::new(harness.fixture.state.clone());
            let _lock = locks
                .lock_volume(&harness.record.volume_uuid)
                .expect("hold requested volume");
            let error = harness
                .run(
                    &bytes_a,
                    &Meter::new(&[4]),
                    &Flusher::ok(),
                    SpacePruneBounds::design(),
                )
                .expect_err("requested volume is locked");
            assert_eq!(error.kind(), ErrorKind::Locked);
            let after = state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
            assert_eq!(
                after.records, journal.records,
                "lock refusal leaves other-volume journal untouched"
            );
            assert!(path_a.is_dir());
        }
        // This volume's pass is neither blocked nor made to replay it.
        let report = harness
            .run(
                &bytes_a,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("this volume's pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        let replay = effect.replay.as_ref().expect("the replay is reported");
        assert_eq!(
            replay.examined, 0,
            "another volume's intent is not this pass's"
        );
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.pruned.rows[0].root_id, root_a);
        assert!(!path_a.exists());
        let journal =
            state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("the journal");
        assert_eq!(
            PendingIntent::pending(&journal.records)
                .iter()
                .filter(|intent| intent.volume_uuid == other_uuid)
                .count(),
            1,
            "the other volume's intent is still pending, untouched"
        );
    }

    /// A pending intent is per object (ruling `698d9ba3`): one this run cannot
    /// settle keeps **its** root and is reported, while the pass deletes the roots
    /// the intent does not cover.
    #[test]
    fn an_unsettled_intent_keeps_its_own_root_and_the_pass_still_prunes_the_rest() {
        let harness = Harness::new("space-prune-per-object-intent", RESERVE, FLOOR);
        let (first, first_path) = harness.seed("pool", 1_000);
        let (second, second_path) = harness.seed("pool", 2_000);
        // The injected fault names one payload, so it stays with the root that owns
        // it and the other root's deletion is not poisoned by the same shift.
        std::fs::rename(
            second_path.join("payload.bin"),
            second_path.join("second.bin"),
        )
        .expect("the second root's payload is renamed");
        let (preview, bytes) = harness.preview();
        assert_eq!(preview.plan.candidates.rows[0].root_id, first);

        // Interrupt the pass on the older root; the fault stays armed, so the next
        // run's replay of that one intent cannot settle it either.
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("the first run is interrupted");
        assert!(
            !first_path.exists(),
            "the first root is in its quarantine leaf"
        );

        let report = harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4, 20]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect("the second run prunes the roots the intent does not cover");
        let effect = report.effect.as_ref().expect("the effect");
        let replay = effect.replay.as_ref().expect("the replay is reported");
        assert_eq!(
            replay.examined, 1,
            "the volume's one pending intent was replayed"
        );
        assert_eq!(replay.pending, 1, "and it is still unsettled");
        assert_eq!(
            held_reason(&report, &first).as_deref(),
            Some("pending_intent"),
            "the root the intent covers is kept and reported"
        );
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(effect.pruned.rows[0].root_id, second);
        assert!(!second_path.exists(), "an unrelated root still yields");
    }

    /// A bound can cut a batch short; the batch is still flushed when it deleted
    /// something, so no deletion is reported from an unflushed measurement.
    #[test]
    fn a_batch_a_bound_cut_short_is_still_flushed() {
        // A 30-byte target with one root per batch and a two-root batch: the roots
        // bound stops the batch after one deletion, and that batch is flushed.
        let harness = Harness::new("space-prune-partial-flush", 30, FLOOR);
        harness.seed("pool", 1_000);
        harness.seed("pool", 2_000);
        let (_, bytes) = harness.preview();
        let bounds = SpacePruneBounds {
            roots: 1,
            ..SpacePruneBounds::design()
        };
        let flusher = Flusher::ok();
        let report = harness
            .run(&bytes, &Meter::new(&[4, 10]), &flusher, bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "budget_exhausted");
        assert_eq!(effect.pruned.count, 1);
        assert_eq!(
            effect.flush.attempts, 1,
            "the partial batch is flushed, not skipped"
        );
        assert_eq!(flusher.calls(), 1);
        assert_eq!(
            effect.after.free_bytes, 10,
            "the report's measurement is the flushed one"
        );
        assert_eq!(effect.not_reached.count, 1);
    }

    /// A hold recorded **after** the interruption keeps the captured object: the
    /// replay re-reads the unit's holds and leaves the intent pending instead of
    /// finishing the deletion. This is the reviewer's reproduced defect, pinned.
    #[test]
    fn a_replay_refuses_to_finish_a_deletion_a_live_hold_covers() {
        for adopted in [false, true] {
            let harness = Harness::new("space-prune-replay-hold", RESERVE, FLOOR);
            let (root_id, path) = if adopted {
                let (id, path, _) = harness.seed_adopted("pool", "adopted-name", 1_000);
                (id, path)
            } else {
                harness.seed("pool", 1_000)
            };
            let (_, bytes) = harness.preview();

            // Interrupt the pass mid-deletion: the root is captured, the intent pending.
            let faults = Faults::new().with_device_shift(
                "payload.bin",
                crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
            );
            harness
                .run_with(
                    &faults,
                    &bytes,
                    &Meter::new(&[4]),
                    &Flusher::ok(),
                    SpacePruneBounds::design(),
                )
                .expect_err("the first run is interrupted");
            assert!(!path.exists(), "the root is in its quarantine leaf");

            // A consumer now records a live hold on the root by the id its marker
            // proves. The root's directory is already captured, so the hold is written
            // as account state, exactly as a hold that raced the capture would be.
            let holds = harness.fixture.state.holds_dir().join(&root_id);
            std::fs::create_dir_all(&holds).expect("the hold directory is made");
            std::fs::set_permissions(&holds, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .expect("the hold directory is private");
            let file = holds.join("consumer.json");
            std::fs::write(
                &file,
                crate::testkit::VolumeFixture::hold_record(
                    &root_id,
                    "consumer",
                    std::process::id(),
                    7_000_000,
                    crate::testkit::FIXTURE_BOOT,
                ),
            )
            .expect("the hold is written");
            std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .expect("the hold is private");

            // The device fault is spent, so only the hold can stop the replay.
            let report = harness
                .run(
                    &bytes,
                    &Meter::new(&[4]),
                    &Flusher::ok(),
                    SpacePruneBounds::design(),
                )
                .expect("the pass runs");
            let effect = report.effect.as_ref().expect("the effect");
            let replay = effect.replay.as_ref().expect("the replay is reported");
            assert_eq!(replay.examined, 1);
            assert_eq!(replay.resumed, 0, "the replay must not resume a held unit");
            assert_eq!(replay.pending, 1, "the held intent stays pending");
            assert_eq!(effect.pruned.count, 0, "nothing is deleted");
            assert_eq!(
                held_reason(&report, &root_id).as_deref(),
                Some("pending_intent"),
                "the root the held intent covers is kept and reported"
            );
            // The captured payload is still inside its quarantine leaf.
            let leaf = std::fs::read_dir(path.parent().expect("the area"))
                .expect("the area reads")
                .flatten()
                .map(|entry| entry.path())
                .find(|entry| {
                    entry
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(crate::state::QUARANTINE_PREFIX))
                })
                .expect("the quarantine leaf is kept");
            assert!(
                leaf.join("payload.bin").is_file(),
                "a live hold keeps the captured object"
            );
            let intents =
                state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
            let pending = PendingIntent::pending(&intents.records);
            assert_eq!(pending.len(), 1);
            assert!(pending[0].intent_id.len() == 32);
        }
    }

    /// Publish a changed registry after preflight's read, before the prune takes
    /// the volume lock. This ordering models a registration winning that lock.
    #[test]
    fn replay_checks_locked_durability_and_reports_effects_on_later_error() {
        struct RegistrationAtMount<'a> {
            harness: &'a Harness,
            invalid: bool,
        }
        impl crate::os::MountRoot for RegistrationAtMount<'_> {
            fn mount_root(
                &self,
                path: &std::path::Path,
            ) -> crate::error::Result<crate::os::MountPoint> {
                if self.invalid {
                    std::fs::write(self.harness.fixture.state.registry_path(), b"{invalid")
                        .expect("corrupt registry after preflight");
                } else {
                    let mut record = self.harness.record.clone();
                    record.durability = Durability::Kept;
                    self.harness.fixture.register_record(&record);
                }
                self.harness.fixture.mounts().mount_root(path)
            }
        }
        struct FailedMeasurement;
        impl SpaceFacts for FailedMeasurement {
            fn measure_space(&self, _: &VerifiedVolume) -> crate::error::Result<SpaceMeasurement> {
                Err(crate::error::Error::new(
                    ErrorKind::VolumeAbsent,
                    "volume disappeared after the replay flush",
                ))
            }
        }
        let harness = Harness::new("space-prune-replay-kept-race", RESERVE, FLOOR);
        let (root_id, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let faults = Faults::new().with_device_shift(
            "payload.bin",
            crate::os::DeviceId::new(harness.fixture.device.get().saturating_add(1)),
        );
        harness
            .run_with(
                &faults,
                &bytes,
                &Meter::new(&[4]),
                &Flusher::ok(),
                SpacePruneBounds::design(),
            )
            .expect_err("interrupt after capture");
        let intents = state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
        let pending = PendingIntent::pending(&intents.records);
        assert_eq!(pending.len(), 1);
        let leaf = path.parent().expect("area").join(format!(
            "{}{}",
            crate::state::QUARANTINE_PREFIX,
            pending[0].intent_id
        ));
        let digest = sha256_hex(&bytes);
        let request = harness.request(&digest, &bytes, SpacePruneBounds::design());
        let registry_bytes = std::fs::read(harness.fixture.state.registry_path())
            .expect("save fixture registry before corruption");
        for invalid in [false, true] {
            harness.fixture.register_record(&harness.record);
            let error = prune(
                &harness.fixture.state,
                &harness.fixture.platform(),
                &harness.facts,
                &RegistrationAtMount {
                    harness: &harness,
                    invalid,
                },
                &harness.observer,
                &Meter::new(&[4]),
                &Flusher::ok(),
                &request,
            )
            .expect_err("changed authority stops before replay");
            assert_eq!(error.kind(), ErrorKind::UnknownState);
            assert!(
                leaf.join("payload.bin").is_file(),
                "replay never touches kept or unknown data"
            );
            let after = state::read_intents(&harness.fixture.state, INTENT_ROWS).expect("journal");
            assert_eq!(after.records, intents.records, "intent remains unchanged");
        }
        std::fs::write(harness.fixture.state.registry_path(), registry_bytes)
            .expect("restore the corrupted fixture registry");
        harness.fixture.register_record(&harness.record);
        let replay_flush = Flusher::ok();
        let error = prune(
            &harness.fixture.state,
            &harness.fixture.platform(),
            &harness.facts,
            &harness.fixture.mounts(),
            &harness.observer,
            &FailedMeasurement,
            &replay_flush,
            &request,
        )
        .expect_err("measurement fails after replay completes");
        assert_eq!(
            error.kind(),
            ErrorKind::UnknownState,
            "a post-replay absence must report the effects at exit 3"
        );
        assert_eq!(error.kind().exit_code(), 3);
        assert!(!leaf.exists(), "replay completed the captured deletion");
        assert_eq!(
            replay_flush.calls(),
            1,
            "completed replay flush precedes even a failed measurement"
        );
        assert_eq!(error.detail()["flush"]["attempts"], 1);
        assert_eq!(error.detail()["replay"]["resumed"], 1);
        assert_eq!(
            error.detail()["replay"]["resumed_root_ids"],
            serde_json::json!([root_id])
        );
        harness.no_pending_intent();
    }

    #[test]
    fn canonical_plan_bytes_and_the_plan_schema_are_the_ones_a_pass_presents() {
        let harness = Harness::new("space-prune-schema", RESERVE, FLOOR);
        harness.seed("pool", 1_000);
        let (preview, bytes) = harness.preview();
        assert!(preview.effect.is_none(), "a preview reports no effect");
        assert_eq!(
            crate::space::read_plan(&bytes)
                .expect("the plan reads")
                .schema,
            crate::space::SPACE_PRUNE_PLAN_SCHEMA
        );
        let encoded = encode(&preview);
        let read = read_prune_report(&encoded).expect("a preview reads");
        assert!(read.preview);
        assert!(read.effect.is_none());
        // `effect` is a required field of this major, present as `null` in a preview:
        // omitting it is a removed field, never "this run did nothing".
        let mut missing = serde_json::from_slice::<serde_json::Value>(&encoded).expect("json");
        missing["data"]
            .as_object_mut()
            .expect("the data is an object")
            .remove("effect");
        assert_eq!(
            read_prune_report(&serde_json::to_vec(&missing).expect("encode"))
                .expect_err("a removed effect field is refused")
                .kind(),
            ErrorKind::UnknownState
        );
        let _ = RowSet::<u8>::empty();
    }
}
