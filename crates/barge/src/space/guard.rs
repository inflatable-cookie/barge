//! The untrusted checkpoint token and the guard's admission decisions.

use crate::area;
use crate::error::{Error, ErrorKind, Result};
use crate::os::VolumeUuid;
use serde::{Deserialize, Serialize};

/// A checkpoint value, carrying lookup keys and report-only observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceGuard {
    /// Registered volume lookup key.
    pub volume_uuid: VolumeUuid,
    /// Area namespace.
    pub namespace: String,
    /// Area name.
    pub area: String,
    /// Last observed policy generation; not authority.
    pub generation: u64,
    /// Last observation time; not authority.
    pub observed_ms: u64,
}
impl SpaceGuard {
    /// Strictly parse a bounded checkpoint token.
    pub fn parse(token: &str) -> Result<Self> {
        let invalid = || Error::new(ErrorKind::Usage, "invalid space guard token");
        if token.len() > 1024 {
            return Err(invalid());
        }
        let mut parts = token.split(':');
        let uuid = parts.next().ok_or_else(invalid)?;
        if uuid.len() != 36 {
            return Err(invalid());
        }
        let volume_uuid = VolumeUuid::parse(uuid).map_err(|_| invalid())?;
        let (namespace, name) = parts
            .next()
            .ok_or_else(invalid)?
            .split_once('/')
            .ok_or_else(invalid)?;
        area::validate_name("namespace", namespace).map_err(|_| invalid())?;
        area::validate_name("area", name).map_err(|_| invalid())?;
        let number = |text: &str| {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            text.parse::<u64>().map_err(|_| invalid())
        };
        let generation = number(parts.next().ok_or_else(invalid)?)?;
        let observed_ms = number(parts.next().ok_or_else(invalid)?)?;
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            volume_uuid,
            namespace: namespace.to_owned(),
            area: name.to_owned(),
            generation,
            observed_ms,
        })
    }
    /// Format this checkpoint as a token.
    pub fn token(&self) -> String {
        format!(
            "{}:{}/{}:{}:{}",
            self.volume_uuid, self.namespace, self.area, self.generation, self.observed_ms
        )
    }
}

/// A space decision cannot name an internal fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceDecision {
    /// Admit above the reserve and floor.
    Admit,
    /// Admit above the floor with a reserve warning.
    AdmitBelowReserve,
    /// A running consumer may continue.
    Continue,
    /// Deletion completed but the floor is breached.
    StopBelowFloor,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn decision_has_no_fallback_variant() {
        assert!(serde_json::from_str::<SpaceDecision>("\"fallback_local\"").is_err());
        assert!(serde_json::from_str::<SpaceDecision>("{\"fallback_local\":\"/tmp\"}").is_err());
    }

    #[test]
    fn checkpoint_token_round_trips_and_rejects_malformed_values() {
        let token = "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool:7:123";
        let guard = SpaceGuard::parse(token).unwrap();
        assert_eq!(SpaceGuard::parse(&guard.token()).unwrap(), guard);
        for invalid in [
            "",
            "x:oxide/pool:1:2",
            "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool:1:2:3",
            "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool/extra:1:2",
            "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool:+1:2",
        ] {
            assert_eq!(
                SpaceGuard::parse(invalid).unwrap_err().kind(),
                ErrorKind::Usage
            );
        }
    }
}

use super::{effect, SpacePruneBounds, SpacePruneRunRequest};
use crate::contract::{RowSet, ROOT_ROWS};
use crate::delete::DeleteBounds;
use crate::os::{
    self, AbsenceVolume, DirOps, Flush, MountRoot, ProcessObserver, SpaceFacts, SpaceMeasurement,
};
use crate::registry::SpacePolicy;
use crate::state::{self, StatePaths};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The schema of a completed guard or recheck.
pub const SPACE_GUARD_SCHEMA: &str = "barge.space.guard/1";

/// Inputs to one guard or recheck.
#[derive(Debug, Clone)]
pub struct GuardRequest<'a> {
    /// Volume registry lookup key.
    pub volume_uuid: &'a VolumeUuid,
    /// Stored namespace.
    pub namespace: &'a str,
    /// Stored area name.
    pub area: &'a str,
    /// Overall wall budget, at most 120 seconds.
    pub wall: Duration,
    /// Whether this call is a running consumer's checkpoint.
    pub recheck: bool,
}

/// Area identity in a guard document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardArea {
    /// Namespace.
    pub namespace: String,
    /// Area name.
    pub area: String,
}

/// A root deleted by the guard, without an internal path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardPruned {
    /// Root identity.
    pub root_id: String,
    /// Owning area.
    pub area: String,
    /// Planned bytes.
    pub bytes: u64,
    /// Last recorded release.
    pub last_release_ms: u64,
}

/// A planned root left untouched when a bound stopped the pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardUnreached {
    /// Root identity.
    pub root_id: String,
    /// Owning area.
    pub area: String,
    /// Planned bytes, not reclaimed bytes.
    pub bytes: u64,
    /// Why it was not reached.
    pub reason: String,
}

/// A root kept because a durable intent still covers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardKept {
    /// Root identity where recorded.
    pub root_id: String,
    /// Why the root is kept.
    pub reason: String,
    /// The pending intent that covers it.
    pub intent_id: String,
}

/// One kept object, without a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardObservedKept {
    /// Area containing the object.
    pub area: String,
    /// Entry name.
    pub name: String,
    /// Root identity when known.
    pub root_id: Option<String>,
    /// Why the object is kept.
    pub reason: String,
}

/// A completed guard decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardReport {
    /// Decision for the consumer.
    pub decision: SpaceDecision,
    /// Registered and verified volume.
    pub volume_uuid: VolumeUuid,
    /// Verified volume mount.
    pub mount_hint: PathBuf,
    /// Area identity.
    pub area: GuardArea,
    /// First measurement.
    pub before: SpaceMeasurement,
    /// Deciding measurement.
    pub after: SpaceMeasurement,
    /// Current policy.
    pub policy: SpacePolicy,
    /// Newly pruned roots.
    pub pruned: RowSet<GuardPruned>,
    /// Planned roots left untouched by the pass.
    pub not_reached: RowSet<GuardUnreached>,
    /// Roots protected by pending intents.
    pub kept: RowSet<GuardKept>,
    /// Kept objects, or null when the volume could not be enumerated.
    pub observed_kept: Option<RowSet<GuardObservedKept>>,
    /// Measured reclaimed bytes.
    pub reclaimed_bytes: u64,
    /// Flush and measurement rounds.
    pub rounds: usize,
    /// A competing prune outlived this guard's lock wait.
    pub decided_after_wait: bool,
    /// Human explanation.
    pub message: String,
    /// Measurements, attribution and effect accounting.
    pub detail: serde_json::Value,
    /// Token for a later recheck.
    pub guard: String,
}

/// Read a completed guard document, rejecting other schemas and invalid counts.
pub fn read_guard_document(bytes: &[u8]) -> Result<GuardReport> {
    let invalid = || {
        Error::new(
            ErrorKind::UnknownState,
            "invalid barge.space.guard/1 document",
        )
    };
    let value = crate::contract::parse_document(bytes)?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some(SPACE_GUARD_SCHEMA)
        || value.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
        || !matches!(
            value.get("command").and_then(serde_json::Value::as_str),
            Some("space guard" | "space recheck")
        )
    {
        return Err(invalid());
    }
    let report: GuardReport =
        serde_json::from_value(value.get("data").ok_or_else(invalid)?.clone())
            .map_err(|_| invalid())?;
    report.pruned.validate()?;
    report.kept.validate()?;
    report.not_reached.validate()?;
    if let Some(rows) = &report.observed_kept {
        rows.validate()?;
    }
    SpaceGuard::parse(&report.guard).map_err(|_| invalid())?;
    let detail = report.detail.as_object().ok_or_else(invalid)?;
    for key in [
        "free_bytes",
        "floor_bytes",
        "reserve_bytes",
        "pruned_root_ids",
        "replayed_root_ids",
        "replay_partial_root_ids",
        "replay_settled_root_ids",
        "reclaimed_bytes",
        "owned_prunable_bytes",
        "held_or_unknown_bytes",
        "excluded_bytes",
        "other_estimate_bytes",
        "other_estimate_label",
        "cause",
        "measurement_source",
        "observed_ms",
    ] {
        if !detail.contains_key(key) {
            return Err(invalid());
        }
    }
    for key in [
        "pruned_root_ids",
        "replayed_root_ids",
        "replay_partial_root_ids",
        "replay_settled_root_ids",
    ] {
        if !detail
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| ids.iter().all(serde_json::Value::is_string))
        {
            return Err(invalid());
        }
    }
    for key in [
        "owned_prunable_bytes",
        "held_or_unknown_bytes",
        "excluded_bytes",
        "other_estimate_bytes",
    ] {
        if !detail
            .get(key)
            .is_some_and(|value| value.is_null() || value.as_u64().is_some())
        {
            return Err(invalid());
        }
    }
    if detail.get("free_bytes").and_then(serde_json::Value::as_u64) != Some(report.after.free_bytes)
        || detail
            .get("floor_bytes")
            .and_then(serde_json::Value::as_u64)
            != Some(report.policy.floor_bytes)
        || detail
            .get("reserve_bytes")
            .and_then(serde_json::Value::as_u64)
            != Some(report.policy.reserve_bytes)
        || detail
            .get("reclaimed_bytes")
            .and_then(serde_json::Value::as_u64)
            != Some(report.reclaimed_bytes)
        || detail
            .get("observed_ms")
            .and_then(serde_json::Value::as_u64)
            != Some(report.after.observed_ms)
        || detail
            .get("measurement_source")
            .and_then(serde_json::Value::as_str)
            != Some(report.after.source.as_str())
        || detail
            .get("other_estimate_label")
            .and_then(serde_json::Value::as_str)
            != Some("estimate")
        || !matches!(
            detail.get("cause").and_then(serde_json::Value::as_str),
            Some(
                "held_owned_roots"
                    | "excluded_or_non_barge"
                    | "unknown"
                    | "none"
                    | "budget_exhausted"
                    | "replay"
            )
        )
    {
        return Err(invalid());
    }
    if detail
        .get("other_estimate_bytes")
        .is_some_and(serde_json::Value::is_null)
        && detail.get("cause").and_then(serde_json::Value::as_str) == Some("none")
    {
        return Err(invalid());
    }
    Ok(report)
}

/// Recheck an untrusted token using current registry, area and volume facts.
#[allow(clippy::too_many_arguments)]
pub fn recheck(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    flush: &dyn Flush,
    token: &SpaceGuard,
    wall: Duration,
) -> Result<GuardReport> {
    guard(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        space,
        flush,
        &GuardRequest {
            volume_uuid: &token.volume_uuid,
            namespace: &token.namespace,
            area: &token.area,
            wall,
            recheck: true,
        },
    )
}

fn measured(space: &dyn SpaceFacts, volume: &os::VerifiedVolume) -> Result<SpaceMeasurement> {
    space.measure_space(volume).map_err(|error| {
        if error.kind() == ErrorKind::VolumeAbsent {
            return error;
        }
        let mut unknown = Error::new(
            ErrorKind::UnknownState,
            format!("space measurement is unknown: {error}"),
        );
        for (key, value) in error.detail() {
            unknown = unknown.with_detail(key, value.clone());
        }
        unknown
    })
}

/// Check the registered volume and current area declaration, then decide admission.
#[allow(clippy::too_many_arguments)]
pub fn guard(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    flush: &dyn Flush,
    request: &GuardRequest<'_>,
) -> Result<GuardReport> {
    guard_with_bounds(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        space,
        flush,
        request,
        SpacePruneBounds::design(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn guard_with_bounds(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    flush: &dyn Flush,
    request: &GuardRequest<'_>,
    bounds: SpacePruneBounds,
) -> Result<GuardReport> {
    let record = area::registered_volume(paths, request.volume_uuid)?;
    let doc = area::show(paths, request.namespace, request.area)?
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "the area is not declared"))?;
    area::resolve(
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &doc.declaration(),
        Some(&doc),
    )?;
    let result = decide_volume(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        space,
        flush,
        request.volume_uuid,
        request.wall,
        request.recheck,
        bounds,
    )?;
    let token = SpaceGuard {
        volume_uuid: request.volume_uuid.clone(),
        namespace: request.namespace.to_owned(),
        area: request.area.to_owned(),
        generation: result.generation,
        observed_ms: result.after.observed_ms,
    }
    .token();
    Ok(GuardReport {
        decision: result.decision,
        volume_uuid: request.volume_uuid.clone(),
        mount_hint: result.mount_hint,
        area: GuardArea {
            namespace: request.namespace.to_owned(),
            area: request.area.to_owned(),
        },
        before: result.before,
        after: result.after,
        policy: result.policy,
        pruned: result.pruned,
        not_reached: result.not_reached,
        kept: result.kept,
        observed_kept: result.observed_kept,
        reclaimed_bytes: result.reclaimed_bytes,
        rounds: result.rounds,
        decided_after_wait: result.decided_after_wait,
        message: result.message,
        detail: result.detail,
        guard: token,
    })
}

/// Volume-wide decision shared by consumer guards and maintenance.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VolumeDecision {
    pub decision: SpaceDecision,
    pub mount_hint: PathBuf,
    pub before: SpaceMeasurement,
    pub after: SpaceMeasurement,
    pub policy: SpacePolicy,
    pub pruned: RowSet<GuardPruned>,
    pub not_reached: RowSet<GuardUnreached>,
    pub kept: RowSet<GuardKept>,
    pub observed_kept: Option<RowSet<GuardObservedKept>>,
    pub reclaimed_bytes: u64,
    pub rounds: usize,
    pub decided_after_wait: bool,
    pub message: String,
    pub detail: serde_json::Value,
    pub generation: u64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn decide_volume(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    flush: &dyn Flush,
    volume_uuid: &VolumeUuid,
    wall: Duration,
    recheck: bool,
    mut bounds: SpacePruneBounds,
) -> Result<VolumeDecision> {
    if wall.is_zero() || wall > Duration::from_secs(120) {
        return Err(Error::new(
            ErrorKind::Usage,
            "guard wall must be greater than zero and at most 120 seconds",
        ));
    }
    let started = Instant::now();
    let record = area::registered_volume(paths, volume_uuid)?;
    if !record.declared_durability()?.permits_pruning() {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            "the volume is kept and does not permit expendable work",
        ));
    }
    let policy = record.declared_space()?.clone();
    let mount = record.mount_hint.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "registered volume has no mount hint",
        )
    })?;
    let verified =
        os::verify_volume(facts, mounts, &mount, &record.expected_volume()).map_err(|error| {
            if error.kind() == ErrorKind::RefusedBoundary {
                Error::new(
                    ErrorKind::VolumeMismatch,
                    "the mounted volume does not match the registered volume",
                )
            } else {
                error
            }
        })?;
    state::read_intents(paths, crate::contract::INTENT_ROWS).map_err(|error| {
        let mut unknown = Error::new(
            ErrorKind::UnknownState,
            format!("intent journal is unreadable: {error}"),
        );
        for (key, value) in error.detail() {
            unknown = unknown.with_detail(key, value.clone());
        }
        unknown
    })?;
    let before = measured(space, &verified)?;
    let mut after = before.clone();
    let mut current_policy = policy;
    let mut generation = record.generation;
    let mut rows = Vec::new();
    let mut not_reached = RowSet::empty();
    let mut reclaimed = 0;
    let mut rounds = 0;
    let mut after_wait = false;
    let mut stop = String::new();
    let mut plan_truncated = false;
    let mut plan_unreached = 0usize;
    let mut replayed_root_ids = Vec::new();
    let mut replay_partial_root_ids = Vec::new();
    let mut replay_settled_root_ids = Vec::new();
    let mut replay_removed_entries = 0usize;
    let mut pass_lock = None;
    if before.free_bytes <= current_policy.reserve_bytes.max(current_policy.floor_bytes) {
        let remaining = wall.saturating_sub(started.elapsed());
        bounds.wall = remaining;
        let run = SpacePruneRunRequest {
            volume_uuid: volume_uuid,
            plan_digest: "",
            plan_bytes: &[],
            now_ms: state::now_ms(),
            lock_wait: Duration::from_secs(5).min(remaining),
            delete_bounds: DeleteBounds::default(),
            bounds,
        };
        match effect::prune_live(
            paths, dirs, facts, mounts, observer, space, flush, &run, None,
        ) {
            Ok((report, lock, replay)) => {
                pass_lock = Some(lock);
                let effect = report.effect.ok_or_else(|| {
                    Error::new(
                        ErrorKind::UnknownState,
                        "live prune did not report its effect",
                    )
                })?;
                after = effect.after;
                current_policy = effect.policy;
                generation = effect.policy_generation;
                reclaimed = effect.reclaimed_bytes;
                rounds = effect.rounds;
                stop = effect.stop;
                plan_truncated = effect.live_plan_truncated;
                plan_unreached = effect.live_plan_unreached;
                let total = effect.not_reached.total;
                let unreached = effect
                    .not_reached
                    .rows
                    .into_iter()
                    .map(|row| GuardUnreached {
                        root_id: row.root_id,
                        area: row.area,
                        bytes: row.bytes,
                        reason: row.reason,
                    })
                    .collect();
                not_reached = RowSet::bounded(unreached, total, ROOT_ROWS);
                replayed_root_ids = replay.resumed_root_ids;
                replayed_root_ids.extend(replay.completed_by_settlement_root_ids);
                replay_partial_root_ids = replay.partial_root_ids;
                replay_settled_root_ids = replay.settled_root_ids;
                replay_removed_entries = replay
                    .entries_removed
                    .saturating_add(replay.partial_entries_removed);
                rows = effect
                    .pruned
                    .rows
                    .into_iter()
                    .map(|row| GuardPruned {
                        root_id: row.root_id,
                        area: row.area,
                        bytes: row.bytes,
                        last_release_ms: row.last_release_ms,
                    })
                    .collect();
            }
            Err(error) if error.kind() == ErrorKind::Locked => {
                after_wait = true;
                let current = area::registered_volume(paths, volume_uuid)?;
                if !current.declared_durability()?.permits_pruning() {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        "volume no longer permits expendable work",
                    ));
                }
                current_policy = current.declared_space()?.clone();
                generation = current.generation;
                let verified =
                    os::verify_volume(facts, mounts, &mount, &current.expected_volume())?;
                after = measured(space, &verified)?;
            }
            Err(error) => return Err(error),
        }
    }
    let intents = state::read_intents(paths, crate::contract::INTENT_ROWS).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!("intent journal is unreadable: {error}"),
        )
        .with_detail(
            "pruned_root_ids",
            serde_json::json!(rows.iter().map(|row| &row.root_id).collect::<Vec<_>>()),
        )
        .with_detail("replayed_root_ids", serde_json::json!(replayed_root_ids))
        .with_detail(
            "replay_partial_root_ids",
            serde_json::json!(replay_partial_root_ids),
        )
        .with_detail(
            "replay_settled_root_ids",
            serde_json::json!(replay_settled_root_ids),
        )
    })?;
    let kept: Vec<GuardKept> = intents
        .records
        .iter()
        .filter(|intent| intent.is_pending() && &intent.volume_uuid == volume_uuid)
        .map(|intent| GuardKept {
            root_id: intent
                .unit
                .root_id
                .clone()
                .unwrap_or_else(|| intent.unit.name.clone()),
            reason: "pending_intent".to_owned(),
            intent_id: intent.intent_id.clone(),
        })
        .collect();
    let attribution = if let Some(lock) = pass_lock.as_ref() {
        let remaining = wall.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            None
        } else {
            Some(
                super::attribute_locked(
                    paths,
                    dirs,
                    facts,
                    mounts,
                    observer,
                    volume_uuid,
                    state::now_ms(),
                    remaining,
                    after.clone(),
                    lock,
                )
                .map_err(|error| {
                    Error::new(
                        ErrorKind::UnknownState,
                        format!("attribution could not be established: {error}"),
                    )
                    .with_detail(
                        "pruned_root_ids",
                        serde_json::json!(rows.iter().map(|row| &row.root_id).collect::<Vec<_>>()),
                    )
                    .with_detail("replayed_root_ids", serde_json::json!(replayed_root_ids))
                    .with_detail(
                        "replay_partial_root_ids",
                        serde_json::json!(replay_partial_root_ids),
                    )
                    .with_detail(
                        "replay_settled_root_ids",
                        serde_json::json!(replay_settled_root_ids),
                    )
                })?,
            )
        }
    } else {
        None
    };
    let observed_kept = attribution
        .as_ref()
        .map(|a| RowSet::bounded(a.kept.clone(), a.kept_total, ROOT_ROWS));
    let prunable = attribution.as_ref().and_then(|a| a.prunable);
    let held = attribution.as_ref().and_then(|a| a.held);
    let excluded = attribution.as_ref().and_then(|a| a.excluded);
    let other = prunable.zip(held).zip(excluded).and_then(|((p, h), e)| {
        after
            .total_bytes
            .checked_sub(after.free_bytes)?
            .checked_sub(p)?
            .checked_sub(h)?
            .checked_sub(e)
    });
    let replay_had_effect = replay_removed_entries > 0 || !replayed_root_ids.is_empty();
    let cause = if replay_had_effect {
        "replay"
    } else if matches!(stop.as_str(), "budget_exhausted" | "space_not_reclaimed") {
        "budget_exhausted"
    } else if other.is_none() {
        "unknown"
    } else if held.unwrap_or(0) > 0 {
        "held_owned_roots"
    } else if excluded.unwrap_or(0) > 0 || other.unwrap_or(0) > 0 {
        "excluded_or_non_barge"
    } else {
        "none"
    };
    let ids: Vec<&str> = rows.iter().map(|row| row.root_id.as_str()).collect();
    let message = format!(
        "volume {} at {}: {} bytes free, floor {} bytes, reserve {} bytes; cause: {} (other bytes are an estimate). No work was sent to the internal drive.",
        volume_uuid,
        mount.display(),
        after.free_bytes,
        current_policy.floor_bytes,
        current_policy.reserve_bytes,
        cause
    );
    let detail = serde_json::json!({"free_bytes":after.free_bytes,"floor_bytes":current_policy.floor_bytes,"reserve_bytes":current_policy.reserve_bytes,"pruned_root_ids":ids,"replayed_root_ids":replayed_root_ids,"replay_partial_root_ids":replay_partial_root_ids,"replay_settled_root_ids":replay_settled_root_ids,"pending_intents":kept,"observed_kept":observed_kept,"not_reached":not_reached,"plan_truncated":plan_truncated,"plan_unreached":plan_unreached,"reclaimed_bytes":reclaimed,"owned_prunable_bytes":prunable,"held_or_unknown_bytes":held,"excluded_bytes":excluded,"other_estimate_bytes":other,"other_estimate_label":"estimate","cause":cause,"measurement_source":after.source,"observed_ms":after.observed_ms,"decided_after_wait":after_wait,"waited_lock":if after_wait { Some(paths.volume_lock_path(volume_uuid).display().to_string()) } else { None },"stop":stop});
    let decision = if after.free_bytes <= current_policy.floor_bytes {
        if rows.is_empty() && !replay_had_effect {
            let mut error = Error::new(ErrorKind::SpaceBelowFloor, message);
            if let Some(fields) = detail.as_object() {
                for (key, value) in fields {
                    error = error.with_detail(key, value.clone());
                }
            }
            return Err(error);
        }
        SpaceDecision::StopBelowFloor
    } else if recheck {
        SpaceDecision::Continue
    } else if after.free_bytes <= current_policy.reserve_bytes {
        SpaceDecision::AdmitBelowReserve
    } else {
        SpaceDecision::Admit
    };
    let total = rows.len();
    let report = VolumeDecision {
        decision,
        mount_hint: mount,
        before,
        after,
        policy: current_policy,
        pruned: RowSet::bounded(rows, total, ROOT_ROWS),
        not_reached,
        kept: RowSet::bounded(kept.clone(), kept.len(), crate::contract::INTENT_ROWS),
        observed_kept,
        reclaimed_bytes: reclaimed,
        rounds,
        decided_after_wait: after_wait,
        message,
        detail,
        generation,
    };
    drop(pass_lock);
    Ok(report)
}
