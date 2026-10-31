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
//!   recorded identity matches. Anything still pending stops the pass before its
//!   first effect.

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
use crate::state::{self, LockSet, StatePaths};
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
    /// How many flushes the pass asked for, one per batch.
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
    /// What the run replayed before its first effect, when it had anything to
    /// replay or settle.
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
    /// How many are still pending, which stops the pass.
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
    let started = Instant::now();
    let deadline = started + request.bounds.wall;
    let effect_deadline = request.bounds.effect_deadline(started);

    // Pre-flight, effect-free: the volume must be registered, its durability must
    // permit pruning and its declared policy must exist. A refusal here changes
    // nothing, so it is an ordinary refusal document.
    let record = area::registered_volume(paths, request.volume_uuid)?;
    let durability = record.declared_durability()?;
    if !durability.permits_pruning() {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "volume {} is declared `{}`, so nothing on it may be pruned for space; nothing was changed",
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

    // Replay first: a real run settles what an earlier one left pending before it
    // deletes anything, and only from the recorded identity, after the volume
    // re-verifies. An intent that stays pending stops this pass before its first
    // effect, because a new delete over an unsettled one is what the journal
    // exists to prevent.
    let replay = delete::replay_pending(
        paths,
        dirs,
        facts,
        observer,
        DeleteBounds {
            deadline: Some(deadline),
            ..request.delete_bounds
        },
        Some(deadline),
    )?;
    if !replay.is_settled() {
        let ids: Vec<&str> = replay
            .pending
            .iter()
            .map(|row| row.intent_id.as_str())
            .collect();
        let rows: Vec<serde_json::Value> = replay
            .pending
            .iter()
            .map(|row| {
                serde_json::json!({
                    "intent_id": row.intent_id,
                    "unit_name": row.unit_name,
                    "unit_kind": row.unit_kind,
                    "phase": row.phase,
                    "reason": row.reason,
                })
            })
            .collect();
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "an earlier space prune left {} intent(s) pending and this run could not settle them, so nothing was deleted; a later run replays them",
                replay.pending.len()
            ),
        )
        .with_detail("pending_intent", serde_json::json!(ids))
        .with_detail("pending", serde_json::json!(rows)));
    }

    // One prune per volume: the volume's exclusive lock is held from this first
    // authoritative measurement to the final decision.
    paths.initialize()?;
    let mut locks = LockSet::with_deadline(
        paths.clone(),
        super::lock_deadline(effect_deadline, request.lock_wait),
    );
    let _volume_lock = locks.lock_volume(request.volume_uuid).map_err(|error| {
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
    let before = space.measure_space(&verified)?;
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
        volume_lock: _volume_lock,
        verified,
        record,
        mount,
        plan,
        plan_sha256: request.plan_digest.to_owned(),
        now_ms: request.now_ms,
        effect_deadline,
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
        flush_row: SpaceFlushRow {
            attempts: 0,
            failures: 0,
            last_ms: 0,
            bound_ms: u64::try_from(request.bounds.flush_deadline.as_millis()).unwrap_or(u64::MAX),
        },
        replay: SpaceReplayRow::from(&replay),
        stop: None,
        batches: 0,
        rounds: 0,
        started_ms: request.now_ms,
    };
    pass.run()?;
    Ok(pass.into_report(started))
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
    volume_lock: crate::state::StateLock,
    verified: VerifiedVolume,
    record: VolumeRecord,
    mount: PathBuf,
    plan: SpacePrunePlan,
    plan_sha256: String,
    now_ms: u64,
    /// The moment no effect may start.
    effect_deadline: Instant,
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
    replay: SpaceReplayRow,
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
        let candidates = self.plan.candidates.rows.clone();
        let mut cursor = 0usize;
        if self.current.free_bytes > self.target() {
            // The volume already satisfies the policy, so there is nothing to
            // reclaim: an operator's real run never deletes data it was not asked
            // to free.
            self.stop = Some("target_reached");
        }
        while self.stop.is_none() {
            if self.pruned.len() >= self.bounds.roots
                || self.batches >= self.bounds.batches
                || Instant::now() >= self.effect_deadline
            {
                self.stop = Some("budget_exhausted");
                break;
            }
            if cursor >= candidates.len() {
                self.stop = Some("nothing_eligible");
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
            for candidate in &batch {
                if self.stop.is_some() || Instant::now() >= self.effect_deadline {
                    // The bound stops this batch where it is; the rows it did not
                    // attempt are reported as unreached below.
                    self.stop = Some("budget_exhausted");
                    break;
                }
                if self.pruned.len() >= self.bounds.roots {
                    self.stop = Some("budget_exhausted");
                    break;
                }
                match self.effect(candidate) {
                    Outcome::Deleted(row) => {
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
            if self.stop.is_some() {
                break;
            }
            // One flush per batch, then re-measure: the reclaim check is a measured
            // fact, never a sum of the sizes the plan recorded.
            if let Err(error) = self.flush_and_measure() {
                return Err(self.annotate(error));
            }
            self.rounds = self.rounds.saturating_add(1);
            if self.stop.is_some() {
                // A flush that outlived its own bound is a reached bound, and the
                // measurement beside it is still the one the report carries.
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
            } else if cursor >= candidates.len() {
                self.stop = Some("nothing_eligible");
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
        for row in &self.plan.candidates.rows {
            if attempted.contains(&row.root_id.as_str()) {
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
        let registry = registry::read(self.paths).map_err(|error| {
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
            self.measure()?;
            if self.current.free_bytes > self.target() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Measure the volume without flushing and adopt the fresh fact.
    fn measure(&mut self) -> Result<()> {
        let measurement = self.space.measure_space(&self.verified).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!("the volume could not be measured again, so this pass stopped: {error}"),
            )
        })?;
        self.current = measurement;
        Ok(())
    }

    /// One batch's flush (30 s bound), the volume reverify around it, and the
    /// re-measurement the reclaim check reads.
    fn flush_and_measure(&mut self) -> Result<()> {
        self.flush_row.attempts = self.flush_row.attempts.saturating_add(1);
        let began = Instant::now();
        let flushed = self.flush.flush_volume(&self.verified);
        let elapsed = began.elapsed();
        self.flush_row.last_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if let Err(error) = flushed {
            self.flush_row.failures = self.flush_row.failures.saturating_add(1);
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the volume flush after this batch failed, so the post-prune measurement is unknown: {error}"
                ),
            ));
        }
        // A drive pulled during the flush is a reverify failure, not a measurement
        // of zero: the post-prune state is unknown and the pass says so.
        self.verified.reverify(self.facts).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the volume could not be re-verified after the flush, so the post-prune measurement is unknown: {error}"
                ),
            )
        })?;
        self.measure()?;
        if elapsed > self.bounds.flush_deadline {
            // One uninterruptible platform call can outlive its bound; the pass then
            // reports a reached bound rather than presenting the batch as complete.
            self.stop = Some("budget_exhausted");
        }
        Ok(())
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
                self.stop = Some("target_reached");
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
            &self.volume_lock,
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
        error
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
            replay: Some(self.replay),
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
    if candidate.area == scratch_label() {
        let handle = dirs.open_path(&row.configured_path).map_err(|error| {
            Skip::new(
                "identity_changed",
                format!(
                    "the scratch root `{}` could not be opened again, so it is kept: {error}",
                    candidate.name
                ),
            )
        })?;
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
    // The durable record must agree with this observation when the root has one.
    // A root that predates the record has none, and that absence is not a
    // disagreement; an unreadable or disagreeing record keeps the root rather than
    // being resolved in either direction.
    let uid = observer.effective_uid().map_err(|error| {
        Skip::new(
            "identity_changed",
            format!("this account's effective uid is unreadable: {error}"),
        )
    })?;
    match identity::verify(paths, dirs, facts, mounts, uid, &candidate.root_id) {
        IdentityVerdict::Verified { .. } => {}
        IdentityVerdict::Unknown {
            reason: IdentityReason::IdentityRecordMissing,
        } => {}
        IdentityVerdict::Unknown { reason } => {
            return Err(Skip::new(
                "identity_changed",
                format!(
                    "the root `{}`'s durable identity record does not verify ({}), so it is kept",
                    candidate.name,
                    identity_reason(reason)
                ),
            ))
        }
        IdentityVerdict::Gone => {
            return Err(Skip::new(
                "identity_changed",
                format!(
                    "the root `{}`'s durable identity record proves the root is gone, so it is kept and reported",
                    candidate.name
                ),
            ))
        }
    }
    // No pending intent may cover this root: the deletion's own custody reads them
    // too, and this check names the fact the report carries.
    let intents = state::read_intents(paths, INTENT_ROWS).map_err(|error| {
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
    use std::time::Duration;

    use super::{prune, SpacePruneBounds, SpacePruneRunRequest};
    use crate::area::{
        self, AreaDeclaration, HoldRequest, HoldScope, ReserveParticipation, RootKind,
    };
    use crate::contract::{RowSet, INTENT_ROWS};
    use crate::custody::PendingIntent;
    use crate::delete::DeleteBounds;
    use crate::digest::sha256_hex;
    use crate::error::ErrorKind;
    use crate::os::{Flush, OpenDescriptor, SpaceFacts, SpaceMeasurement, VerifiedVolume};
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

        fn failing() -> Self {
            Self {
                fail: Cell::new(true),
                calls: Cell::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    impl Flush for Flusher {
        fn flush_volume(&self, _volume: &VerifiedVolume) -> crate::error::Result<()> {
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

        fn release_at(&self, root_id: &str, at: u64, created: u64) {
            area::hold(
                &self.fixture.state,
                &self.observer,
                &HoldRequest {
                    root_id: root_id.to_owned(),
                    holder: "prover".to_owned(),
                    pid: std::process::id(),
                    scope: HoldScope::Write,
                    purpose: "a fixture holder".to_owned(),
                },
            )
            .expect("the hold is written");
            area::release(&self.fixture.state, &self.observer, root_id, "prover")
                .expect("the hold releases");
            let path = self
                .fixture
                .state
                .activity_path(root_id)
                .expect("the record path");
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).expect("the record reads"))
                    .expect("the record parses");
            value["last_release_ms"] = at.into();
            value["recorded_ms"] = at.into();
            value["first_created_ms"] = created.into();
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&value).expect("the record encodes"),
            )
            .expect("the record is written");
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
        assert_eq!(effect.pruned.count, 1);
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
        // still needs the prune, so the pass adopts it and deletes.
        let record = Harness::register(&harness.fixture, 32, 8);
        assert_ne!(record.generation, harness.record.generation);
        let report = harness
            .run(
                &bytes,
                &Meter::new(&[4, 4, 40]),
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
    fn a_reserve_lowered_below_the_measurement_keeps_the_root_and_stops() {
        let harness = Harness::new("space-prune-reserve-down", RESERVE, FLOOR);
        let (root, path) = harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        // Reserve 4, floor 2: the fresh measurement (20) already satisfies the new
        // policy, so the candidate no longer qualifies and nothing is deleted.
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
        assert_eq!(effect.pruned.count, 0, "{effect:?}");
        assert_eq!(effect.stop, "target_reached");
        assert_eq!(
            held_reason(&report, &root).as_deref(),
            Some("policy_changed")
        );
        assert!(path.is_dir());
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
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
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

    /// The failure matrix's **signal** row, carried by the durable order rather
    /// than a handler: Barge installs no signal handler, and the intent is durable
    /// before the first destructive step, so an interruption at exactly this point
    /// — a process that stopped between publishing its intent and finishing the
    /// walk — leaves a replayable intent and never an unrecorded effect. A killed
    /// process exits non-zero by the operating system's doing; the next run replays
    /// the intent.
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
        let replay = effect.replay.expect("the replay is reported");
        assert_eq!(replay.examined, 1);
        assert_eq!(replay.pending, 0);
        assert!(replay.resumed + replay.settled == 1, "the intent settled");
        assert_eq!(effect.pruned.count, 1);
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
        // The state volume refuses the intent write: `ENOSPC` and a read-only state
        // volume take the same path, and it is the same refusal the failure matrix
        // asks for — nothing deleted, and the refusal names the state volume.
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

    #[test]
    fn a_zero_flush_bound_is_reported_as_a_reached_bound() {
        let harness = Harness::new("space-prune-flush-bound", RESERVE, FLOOR);
        harness.seed("pool", 1_000);
        let (_, bytes) = harness.preview();
        let bounds = SpacePruneBounds {
            flush_deadline: Duration::ZERO,
            ..SpacePruneBounds::design()
        };
        let report = harness
            .run(&bytes, &Meter::new(&[4, 20]), &Flusher::ok(), bounds)
            .expect("the pass runs");
        let effect = report.effect.as_ref().expect("the effect");
        assert_eq!(effect.stop, "budget_exhausted");
        assert_eq!(effect.pruned.count, 1);
        assert!(report.needs_human());
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
        let _ = RowSet::<u8>::empty();
    }
}
