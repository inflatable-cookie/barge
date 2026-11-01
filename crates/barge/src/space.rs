//! Read-only measurements of registered volumes, and the space prune: its
//! candidate plan and, in [`effect`], its real run.

mod effect;
mod guard;
pub(crate) use guard::decide_volume;

pub use guard::{
    guard, read_guard_document, recheck, GuardArea, GuardKept, GuardObservedKept, GuardPruned,
    GuardReport, GuardRequest, GuardUnreached, SpaceDecision, SpaceGuard, SPACE_GUARD_SCHEMA,
};

pub use effect::{
    prune, SpaceFlushRow, SpacePruneBounds, SpacePruneEffect, SpacePruneHeldRow,
    SpacePruneRunRequest, SpacePrunedRow, SpaceReplayRow, SpaceUnreachedRow, SPACE_FLUSH_DEADLINE,
    SPACE_PRUNE_BATCHES, SPACE_PRUNE_BATCH_ROOTS, SPACE_PRUNE_ROOTS, SPACE_PRUNE_ROOT_DEADLINE,
    SPACE_PRUNE_STOP_MARGIN, SPACE_PRUNE_WALL,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::area::{
    self, AreaDocument, IdleParticipation, LruKey, MarkerExpectation, MarkerState, MarkerStateName,
    ReserveParticipation, RootKind,
};
use crate::contract::{RowSet, AREA_ROWS, DOCUMENT_SIZE_CAP_BYTES, ROOT_ROWS, VOLUME_ROWS};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    self, DirHandle, DirOps, MountRoot, ProcessObserver, SpaceFacts, SpaceMeasurement, VolumeFacts,
    VolumeUuid,
};
use crate::registry::{self, VolumeRecord};
use crate::scratch::{self, SweepBounds};
use crate::state::{self, Deadline, LockRank, LockSet, StateLock, StatePaths, READ_DEADLINE};
use crate::unit;

/// Version of the read-only space report.
pub const SPACE_SCHEMA: &str = "barge.space/1";

/// Version of the space-prune plan and its preview report (stage 3, slice T4).
pub const SPACE_PRUNE_SCHEMA: &str = "barge.space.prune/1";

/// Version of the canonical plan the digest covers and a real run presents.
pub const SPACE_PRUNE_PLAN_SCHEMA: &str = "barge.space.prune.plan/1";

/// How long one candidate plan may take (stage 3 design, "Bounded resources").
pub const SPACE_PLAN_WALL: Duration = Duration::from_secs(10);

/// One shared-lock wait, bounded by both the plan's own lock budget and what is
/// left of the plan's wall clock.
///
/// A fresh `lock_wait` per call would let a lock taken near the end of the wall
/// extend the preview past its advertised bound; carrying ruling `6ac32f24`
/// requires every shared wait inside the run's deadline. A zero remaining budget
/// is already expired, so a busy object at the wall boundary is refused at once.
fn lock_deadline(deadline: Instant, lock_wait: Duration) -> Deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    Deadline::after(remaining.min(lock_wait))
}

/// Measurements at the time of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceReport {
    /// Time the report completed.
    pub observed_ms: u64,
    /// Registered volumes, bounded by the registry limit.
    pub volumes: RowSet<SpaceRow>,
    /// Observed fallback roots, or absent in older documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_roots: Option<RowSet<FallbackRoot>>,
}

/// A report-only observation of a declared fallback path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackRoot {
    pub volume_uuid: VolumeUuid,
    pub namespace: String,
    pub area: String,
    pub path: PathBuf,
    pub reason: String,
}

/// Observe fallbacks without following links or creating paths.
pub(crate) fn fallback_roots(
    paths: &StatePaths,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    records: &[VolumeRecord],
) -> Result<RowSet<FallbackRoot>> {
    let declarations = area::read_declarations(paths)?;
    let deadline = Instant::now() + READ_DEADLINE;
    let mut rows = Vec::new();
    let mut total = 0usize;
    for record in records {
        for declaration in &declarations.documents {
            let area::AbsentPolicy::FallbackLocal { path } = &declaration.absent else {
                continue;
            };
            if Instant::now() >= deadline || total >= ROOT_ROWS {
                return Ok(RowSet::bounded(rows, total.saturating_add(1), ROOT_ROWS));
            }
            let observation = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => false,
            };
            let reason = if !observation {
                "unknown"
            } else if let Some(mount) = &record.mount_hint {
                match os::verify_volume(facts, mounts, mount, &record.expected_volume()) {
                    Ok(_) => "fallback_while_present",
                    Err(error) if error.kind() == ErrorKind::VolumeAbsent => "volume_absent",
                    Err(_) => "unknown",
                }
            } else {
                "unknown"
            };
            total += 1;
            rows.push(FallbackRoot {
                volume_uuid: record.volume_uuid.clone(),
                namespace: declaration.namespace.clone(),
                area: declaration.area.clone(),
                path: path.clone(),
                reason: reason.to_owned(),
            });
        }
    }
    if declarations.truncated && !records.is_empty() {
        // The unexamined declarations may include fallbacks; never report a
        // complete empty observation when their contents are unknown.
        return Ok(RowSet::bounded(rows, total.saturating_add(1), total));
    }
    Ok(RowSet::bounded(rows, total, ROOT_ROWS))
}

/// A registered volume and its present measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceRow {
    /// Registered identity.
    pub volume_uuid: VolumeUuid,
    /// Verified mount root, when present.
    pub mount_point: Option<PathBuf>,
    /// Kernel measurement, absent only when the volume is absent.
    pub measurement: Option<SpaceMeasurement>,
    /// The declared policy, or none when undeclared.
    pub policy: Option<registry::SpacePolicy>,
    /// Whether the declared floor is below the default.
    #[serde(default)]
    pub floor_below_default: bool,
    /// Current state of the volume and its policy.
    pub state: String,
    /// Area accounting, unavailable until a later slice.
    pub areas: Option<RowSet<AreaSpace>>,
    /// Scratch accounting, unavailable until a later slice.
    pub scratch: Option<ScratchSpace>,
    /// Cache accounting, unavailable until a later slice.
    pub caches: Option<CacheSpace>,
    /// Attribution, unavailable until a later slice.
    pub attribution: Option<Attribution>,
}

/// Space attributed to one area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaSpace {
    /// Owning namespace.
    pub namespace: String,
    /// Area name.
    pub area: String,
    /// Reserve participation.
    pub reserve: String,
    /// Owned bytes.
    pub bytes: u64,
    /// Idle roots.
    pub idle_roots: u64,
    /// Held roots.
    pub held_roots: u64,
    /// Roots with unknown activity.
    pub unknown_roots: u64,
}

/// Space attributed to scratch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchSpace {
    /// Owned bytes.
    pub bytes: u64,
    /// Idle eligible roots.
    pub idle_eligible_roots: u64,
    /// Held roots.
    pub held_roots: u64,
    /// Unconcluded roots.
    pub unconcluded_roots: u64,
}

/// Space attributed to caches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSpace {
    /// Owned bytes.
    pub bytes: u64,
    /// Reserve participation.
    pub reserve: String,
}

/// Attribution of volume use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// Owned bytes.
    pub owned_bytes: u64,
    /// Excluded bytes.
    pub excluded_bytes: u64,
    /// Held or unknown bytes.
    pub held_or_unknown_bytes: u64,
    /// Estimated remaining bytes.
    pub other_estimate_bytes: u64,
    /// Described cause.
    pub cause: String,
}

/// Read the registry and measure present volumes without creating state or locks.
pub fn report(
    paths: &StatePaths,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    space: &dyn SpaceFacts,
) -> Result<SpaceReport> {
    let mut locks = LockSet::new(paths.clone());
    let _lock = locks.observe_registry()?;
    let registry = registry::read(paths)?;
    drop(_lock);
    let mut rows = Vec::new();
    for record in registry.volumes() {
        let _volume_lock = StateLock::shared_if_present(
            &paths.volume_lock_path(&record.volume_uuid),
            LockRank::Volume,
            Deadline::after(READ_DEADLINE),
        )?;
        let path = record.mount_hint.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                "registered volume has no mount hint",
            )
        })?;
        let verified = match os::verify_volume(facts, mounts, path, &record.expected_volume()) {
            Ok(volume) => volume,
            Err(error) if error.kind() == ErrorKind::VolumeAbsent => {
                rows.push(SpaceRow {
                    volume_uuid: record.volume_uuid.clone(),
                    mount_point: None,
                    measurement: None,
                    policy: record.space.clone(),
                    floor_below_default: record
                        .space
                        .as_ref()
                        .is_some_and(|p| p.floor_bytes < registry::DEFAULT_FLOOR_BYTES),
                    state: "volume_absent".to_owned(),
                    areas: None,
                    scratch: None,
                    caches: None,
                    attribution: None,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let measurement = space.measure_space(&verified)?;
        let state = match &record.space {
            None => "policy_undeclared",
            Some(_) if !record.durability.permits_pruning() => "durability_forbids",
            Some(policy) if measurement.free_bytes <= policy.floor_bytes => "below_floor",
            Some(policy) if measurement.free_bytes <= policy.reserve_bytes => "below_reserve",
            Some(_) => "above_reserve",
        };
        rows.push(SpaceRow {
            volume_uuid: record.volume_uuid.clone(),
            mount_point: Some(verified.identity().mount_point.clone()),
            measurement: Some(measurement),
            policy: record.space.clone(),
            floor_below_default: record
                .space
                .as_ref()
                .is_some_and(|p| p.floor_bytes < registry::DEFAULT_FLOOR_BYTES),
            state: state.to_owned(),
            areas: None,
            scratch: None,
            caches: None,
            attribution: None,
        });
    }
    let total = rows.len();
    Ok(SpaceReport {
        observed_ms: state::now_ms(),
        volumes: RowSet::bounded(rows, total, VOLUME_ROWS),
        fallback_roots: Some(fallback_roots(paths, facts, mounts, registry.volumes())?),
    })
}

/// Read a successful space envelope, refusing an unknown major or missing fields.
pub fn read_document(bytes: &[u8]) -> Result<SpaceReport> {
    let value = crate::contract::parse_document(bytes)?;
    let invalid = || Error::new(ErrorKind::UnknownState, "invalid barge.space/1 document");
    if value.get("schema").and_then(|v| v.as_str()) != Some(SPACE_SCHEMA)
        || value.get("command").and_then(|v| v.as_str()) != Some("space")
        || value.get("ok").and_then(|v| v.as_bool()) != Some(true)
    {
        return Err(invalid());
    }
    let data = value.get("data").ok_or_else(invalid)?;
    let rows = data
        .get("volumes")
        .and_then(|v| v.get("rows"))
        .and_then(|v| v.as_array())
        .ok_or_else(invalid)?;
    for row in rows {
        for field in [
            "volume_uuid",
            "mount_point",
            "measurement",
            "policy",
            "state",
            "areas",
            "scratch",
            "caches",
            "attribution",
        ] {
            if row.get(field).is_none() {
                return Err(invalid());
            }
        }
    }
    let report: SpaceReport = serde_json::from_value(data.clone()).map_err(|_| invalid())?;
    report.volumes.validate()?;
    for row in &report.volumes.rows {
        if let Some(areas) = &row.areas {
            areas.validate()?;
        }
        if !matches!(
            row.state.as_str(),
            "above_reserve"
                | "below_reserve"
                | "below_floor"
                | "policy_undeclared"
                | "volume_absent"
                | "space_unknown"
                | "durability_forbids"
        ) {
            return Err(invalid());
        }
    }
    Ok(report)
}

/// Read one `barge.space.prune/1` from its own bytes: the preview or the real
/// run's report.
///
/// It refuses an unknown major, an envelope that is not this command's, and a body
/// that does not provide exactly the field set this major defines, so a removed
/// field is never read as absent and an unknown one is never ignored. A real run's
/// `effect` is `null` in a preview and populated by a pass, and both read.
pub fn read_prune_report(bytes: &[u8]) -> Result<SpacePruneReport> {
    let value = crate::contract::parse_document(bytes)?;
    let invalid = || {
        Error::new(
            ErrorKind::UnknownState,
            format!("invalid {SPACE_PRUNE_SCHEMA} document"),
        )
    };
    let found = value
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    if found != SPACE_PRUNE_SCHEMA {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{found}` is an unknown major; this reader knows {SPACE_PRUNE_SCHEMA}"),
        )
        .with_detail("found", found.into())
        .with_detail("supported", SPACE_PRUNE_SCHEMA.into()));
    }
    if value.get("command").and_then(serde_json::Value::as_str) != Some("space.prune")
        || value.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
    {
        return Err(invalid());
    }
    let data = value.get("data").ok_or_else(invalid)?;
    // `effect` is a required field of this major even though it is `null` in a
    // preview: a document that omits it is a removed field, and absence is never
    // the same fact as "this run did nothing".
    if !data
        .as_object()
        .is_some_and(|body| body.contains_key("effect"))
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the document omits `effect`, a required field of {SPACE_PRUNE_SCHEMA}; a preview states it as null"
            ),
        ));
    }
    let report: SpacePruneReport = serde_json::from_value(data.clone()).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "the document does not provide the field set {SPACE_PRUNE_SCHEMA} requires: {error}"
            ),
        )
    })?;
    report.plan.candidates.validate()?;
    if report.plan.schema != SPACE_PRUNE_PLAN_SCHEMA {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the report's plan is `{}`, not {SPACE_PRUNE_PLAN_SCHEMA}",
                report.plan.schema
            ),
        ));
    }
    if let Some(effect) = &report.effect {
        effect.pruned.validate()?;
        effect.held.validate()?;
        effect.not_reached.validate()?;
    }
    Ok(report)
}

/// What one caller asks the space-prune planner for.
#[derive(Debug, Clone, Copy)]
pub struct SpacePruneRequest<'a> {
    /// The registered volume to plan on.
    pub volume_uuid: &'a VolumeUuid,
    /// The plan's own clock, so a test can place a release in the past.
    pub now_ms: u64,
    /// The plan's wall clock (stage 3 design, "Bounded resources").
    pub wall: Duration,
    /// How long one shared lock may wait for a live owner. It is the plan's own
    /// budget for a lock: a holder past it is refused, never waited out twice
    /// (carried ruling `6ac32f24`).
    pub lock_wait: Duration,
}

/// One candidate the plan would delete, in delete order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceCandidateRow {
    /// The root the candidate is, which is its root id.
    pub root_id: String,
    /// The area it belongs to, as `namespace/area`; scratch is `barge/scratch`.
    pub area: String,
    /// Its directory name inside its folder.
    pub name: String,
    /// The absolute path the walk reached.
    pub path: PathBuf,
    /// The whole-tree allocated bytes a complete scan counted.
    pub bytes: u64,
    /// The root's last recorded release, which orders the candidate.
    pub last_release_ms: u64,
    /// The root's recorded creation time; `0` when it was never recorded.
    pub first_created_ms: u64,
    /// The root directory's device.
    pub device: u64,
    /// The root directory's inode on that device.
    pub inode: u64,
    /// The SHA-256 of the root's exact marker bytes.
    pub marker_digest: String,
    /// The SHA-256 over device, inode and marker digest, which is the identity a
    /// real run re-verifies before its effect.
    pub identity_digest: String,
}

/// One Barge-owned root the plan keeps, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceKeptRow {
    /// The area it belongs to, as `namespace/area`.
    pub area: String,
    /// Its directory name inside its folder.
    pub name: String,
    /// The absolute path the walk reached, when one was resolved.
    pub path: Option<PathBuf>,
    /// The root id, when the marker proved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// What it is: `owned`, `absent` or `unknown`.
    pub state: String,
    /// Why it is kept.
    pub reason: String,
}

/// One area's (or Barge's scratch folder's) participation in the plan.
///
/// A count is `null` when it was not enumerated: an excluded area is never
/// listed, so reporting `0` roots would claim it is empty. Absence of a count is
/// never a zero and never a promise that nothing is there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceParticipationRow {
    /// The area or feature, as `namespace/area`.
    pub area: String,
    /// Its reserve participation: `counts`, `excluded` or `by_construction`.
    pub reserve: String,
    /// `participating`, `excluded`, `absent`, `unknown` or `unreached`.
    pub state: String,
    /// How many root rows the listing examined. It is an explicit `null` when
    /// the area was not enumerated, so a reader can never mistake an unlisted
    /// area for an empty one.
    #[serde(default)]
    pub roots: Option<u64>,
    /// How many candidates this plan took from it, or `null` when not enumerated.
    #[serde(default)]
    pub candidates: Option<u64>,
    /// How many roots it kept, with the reasons in `kept`, or `null` when not
    /// enumerated.
    #[serde(default)]
    pub kept: Option<u64>,
    /// Why it was not enumerated, or what bound stopped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The canonical plan: exactly the bytes the digest covers and a real run must
/// present. No measurement, time or accounting is inside it, so a fresh
/// measurement never invalidates a plan whose candidates are still valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacePrunePlan {
    /// Always `barge.space.prune.plan/1`.
    pub schema: String,
    /// The registered volume.
    pub volume_uuid: VolumeUuid,
    /// The registry record's generation when the plan was built, so a later
    /// effect can refuse a superseded policy.
    pub policy_generation: u64,
    /// The declared space policy the plan applied.
    pub policy: registry::SpacePolicy,
    /// The prune target: `max(reserve_bytes, floor_bytes)`.
    pub target_bytes: u64,
    /// The candidates, in least-recently-used order.
    pub candidates: RowSet<SpaceCandidateRow>,
}

/// The `barge.space.prune/1` body: the plan, its digest and everything beside it
/// that the plan cannot carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacePruneReport {
    /// Whether this run only previewed. A preview changes nothing, and this build
    /// has no effect form.
    pub preview: bool,
    /// When the plan was built. It is outside the plan, so the digest is stable.
    pub planned_ms: u64,
    /// The registered volume.
    pub volume_uuid: VolumeUuid,
    /// The volume's proved mount point.
    pub mount_point: PathBuf,
    /// The declared policy the plan applied.
    pub policy: registry::SpacePolicy,
    /// The registry record's generation the plan read.
    pub policy_generation: u64,
    /// The measurement beside the plan; it is never inside the plan's bytes.
    pub measurement: SpaceMeasurement,
    /// `above_reserve`, `below_reserve` or `below_floor`, from the measurement.
    pub state: String,
    /// The prune target: `max(reserve_bytes, floor_bytes)`.
    pub target_bytes: u64,
    /// How many candidates the plan holds.
    pub candidates: usize,
    /// The SHA-256 of the plan's canonical bytes.
    pub plan_sha256: String,
    /// The stable plan.
    pub plan: SpacePrunePlan,
    /// Every declared area's and Barge's scratch folder's participation.
    pub participation: RowSet<SpaceParticipationRow>,
    /// The roots the plan keeps, with their reasons.
    pub kept: RowSet<SpaceKeptRow>,
    /// How many entries the listings examined.
    pub examined: usize,
    /// Whether a bound stopped the plan before the volume was exhausted.
    pub truncated: bool,
    /// Why a bound stopped it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
    /// How many root rows the bound left unexamined.
    pub unreached: usize,
    /// What a real run did, and what it did not reach. It is `null` for a preview,
    /// which changes nothing (stage 3, slice T5).
    ///
    /// It is a **required** field of this major, present as `null` in a preview: a
    /// reader of `barge.space.prune/1` refuses a document that simply omits it,
    /// because absence is never the same fact as "this run did nothing".
    pub effect: Option<SpacePruneEffect>,
}

impl SpacePruneReport {
    /// Whether this document is a real run whose outcome needs an operator: a pass
    /// that ended on a bound, or one that deleted data for no measured reclaim.
    pub fn needs_human(&self) -> bool {
        self.effect
            .as_ref()
            .is_some_and(SpacePruneEffect::needs_human)
    }
}

/// Build the space-prune candidate plan for one registered volume.
///
/// The plan is effect-free. It reads the volume's declared policy, measures it,
/// lists every declared area whose `reserve` is `counts` and Barge's own scratch
/// folder, and orders the idle roots by the volume-wide least-recently-used key.
/// An area whose `reserve` is `excluded`, and Barge's caches, never contribute a
/// candidate. A root is a candidate only when every input positively concluded
/// its idleness and a complete bounded scan counted its bytes; anything else is
/// kept with the reason, and one undecidable root or area never stops the rest.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    space: &dyn SpaceFacts,
    request: &SpacePruneRequest<'_>,
) -> Result<SpacePruneReport> {
    let started = Instant::now();
    // The lock order the carried ruling `6ac32f24` fixes: the registry's shared
    // observation lock first, released before any walk, then the volume's shared
    // lock for the rest of the pass, then one root's shared lock at a time. A
    // registration write is never blocked by a preview, and a real prune of this
    // volume cannot interleave with it.
    let (record, declarations) = {
        let mut locks = LockSet::with_deadline(
            paths.clone(),
            lock_deadline(started + request.wall, request.lock_wait),
        );
        let _registry_lock = locks.observe_registry()?;
        let record = area::registered_volume(paths, request.volume_uuid)?;
        let declarations = area::read_declarations(paths)?;
        (record, declarations)
    };
    let durability = record.declared_durability()?;
    if !durability.permits_pruning() {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "volume {} is declared `kept`, so nothing on it may be pruned for space",
                record.volume_uuid
            ),
        )
        .with_detail("volume_uuid", record.volume_uuid.as_str().into())
        .with_detail("durability", durability_name(durability).into()));
    }
    let policy = record.declared_space()?.clone();
    let mount = record.mount_hint.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "registered volume has no mount hint",
        )
    })?;
    let verified = os::verify_volume(facts, mounts, &mount, &record.expected_volume())?;
    let measurement = space.measure_space(&verified)?;
    let state = state_name(&measurement, &policy).to_owned();
    let target_bytes = policy.reserve_bytes.max(policy.floor_bytes);
    // A preview takes the volume's shared lock only when the lock file already
    // exists, and creates nothing. A real prune holds the same file exclusively,
    // so it can never be read mid-prune; a holder that outlives this plan's own
    // wait refuses the plan with `locked` and names the volume's lock file. A
    // root that is busy is a different answer: it keeps that root alone.
    let volume_lock = {
        let mut locks = LockSet::with_deadline(
            paths.clone(),
            lock_deadline(started + request.wall, request.lock_wait),
        );
        match locks.observe_volume(&record.volume_uuid) {
            Ok(lock) => lock,
            Err(error) => {
                return Err(Error::new(
                    ErrorKind::Locked,
                    format!(
                        "volume {}'s lock could not be observed, so this volume's space plan was refused: {}; nothing was changed",
                        record.volume_uuid,
                        error.message()
                    ),
                )
                .with_detail(
                    "volume_uuid",
                    record.volume_uuid.as_str().into(),
                )
                .with_detail(
                    "lock",
                    paths
                        .volume_lock_path(&record.volume_uuid)
                        .display()
                        .to_string()
                        .into(),
                ));
            }
        }
    };
    let report = plan_from_snapshot(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        request,
        started,
        record,
        declarations,
        verified,
        measurement,
        mount,
        policy,
        target_bytes,
        state,
    );
    drop(volume_lock);
    report
}

/// Build a candidate plan using the caller's existing volume-lock custody.
#[allow(clippy::too_many_arguments)]
fn plan_from_snapshot(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &SpacePruneRequest<'_>,
    started: Instant,
    record: VolumeRecord,
    declarations: area::DeclarationSet,
    verified: os::VerifiedVolume,
    measurement: SpaceMeasurement,
    mount: PathBuf,
    policy: registry::SpacePolicy,
    target_bytes: u64,
    state: String,
) -> Result<SpacePruneReport> {
    let declarations_truncated = declarations.truncated;
    let mut planner = Planner {
        paths,
        dirs,
        facts,
        mounts,
        observer,
        record: &record,
        device: verified.identity().device,
        now_ms: request.now_ms,
        wall: request.wall,
        lock_wait: request.lock_wait,
        deadline: started + request.wall,
        stopped: None,
        truncated: declarations_truncated,
        examined: 0,
        unreached: 0,
        candidates: BTreeMap::new(),
        candidate_total: 0,
        kept: Vec::new(),
        kept_total: 0,
        participation: Vec::new(),
    };
    for document in &declarations.documents {
        planner.area(document);
    }
    planner.scratch();
    let Planner {
        stopped,
        truncated,
        examined,
        unreached,
        candidates,
        candidate_total,
        kept,
        kept_total,
        participation,
        ..
    } = planner;
    // A `BTreeMap` over the LRU key iterates oldest first, so the bounded plan is
    // already in delete order.
    let rows: Vec<SpaceCandidateRow> = candidates.into_values().collect();
    let plan = SpacePrunePlan {
        schema: SPACE_PRUNE_PLAN_SCHEMA.to_owned(),
        volume_uuid: record.volume_uuid.clone(),
        policy_generation: record.generation,
        policy: policy.clone(),
        target_bytes,
        candidates: RowSet::bounded(rows, candidate_total, ROOT_ROWS),
    };
    let plan_sha256 = sha256_hex(&canonical_plan_bytes(&plan)?);
    let participation_total = participation.len();
    let truncated = truncated || plan.candidates.truncated || kept_total > ROOT_ROWS;
    Ok(SpacePruneReport {
        preview: true,
        planned_ms: request.now_ms,
        volume_uuid: record.volume_uuid.clone(),
        mount_point: mount,
        policy,
        policy_generation: record.generation,
        measurement,
        state,
        target_bytes,
        candidates: plan.candidates.count,
        plan_sha256,
        plan,
        participation: RowSet::bounded(participation, participation_total, AREA_ROWS),
        kept: RowSet::bounded(kept, kept_total, ROOT_ROWS),
        examined,
        truncated,
        truncated_reason: stopped.or_else(|| {
            truncated.then(|| {
                if declarations_truncated {
                    "the declaration list stopped at its row bound, so some areas were not planned"
                        .to_owned()
                } else {
                    format!(
                        "a plan bound stopped the walk; the counts report what was not reached ({unreached} root rows), never that the volume was empty"
                    )
                }
            })
        }),
        unreached,
        effect: None,
    })
}

/// Re-plan under a guard's existing exclusive volume lock without another measurement.
#[allow(clippy::too_many_arguments)]
fn plan_locked(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &SpacePruneRequest<'_>,
    measurement: SpaceMeasurement,
    lock: &StateLock,
) -> Result<SpacePruneReport> {
    if lock.path() != paths.volume_lock_path(request.volume_uuid)
        || lock.rank() != LockRank::Volume
        || lock.mode() != state::LockMode::Exclusive
    {
        return Err(Error::new(
            ErrorKind::Usage,
            "live space planning requires the volume's exclusive lock",
        ));
    }
    let started = Instant::now();
    let record = area::registered_volume(paths, request.volume_uuid)?;
    let durability = record.declared_durability()?;
    if !durability.permits_pruning() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "volume no longer permits pruning",
        ));
    }
    let policy = record.declared_space()?.clone();
    let declarations = area::read_declarations(paths)?;
    let mount = record.mount_hint.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "registered volume has no mount hint",
        )
    })?;
    let verified = os::verify_volume(facts, mounts, &mount, &record.expected_volume())?;
    let state = state_name(&measurement, &policy).to_owned();
    let target_bytes = policy.reserve_bytes.max(policy.floor_bytes);
    plan_from_snapshot(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        request,
        started,
        record,
        declarations,
        verified,
        measurement,
        mount,
        policy,
        target_bytes,
        state,
    )
}

/// Bounded best-effort attribution. Missing values mean the evidence was incomplete.
struct GuardAttribution {
    prunable: Option<u64>,
    held: Option<u64>,
    excluded: Option<u64>,
    kept: Vec<GuardObservedKept>,
    kept_total: usize,
}

#[allow(clippy::too_many_arguments)]
fn attribute_locked(
    paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    uuid: &VolumeUuid,
    now_ms: u64,
    wall: Duration,
    measurement: SpaceMeasurement,
    lock: &StateLock,
) -> Result<GuardAttribution> {
    let started = Instant::now();
    let request = SpacePruneRequest {
        volume_uuid: uuid,
        now_ms,
        wall: wall.min(SPACE_PLAN_WALL),
        lock_wait: Duration::ZERO,
    };
    let report = plan_locked(
        paths,
        dirs,
        facts,
        mounts,
        observer,
        &request,
        measurement,
        lock,
    )?;
    let record = area::registered_volume(paths, uuid)?;
    let verified = os::verify_volume(
        facts,
        mounts,
        record
            .mount_hint
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::UnknownState, "volume mount is unknown"))?,
        &record.expected_volume(),
    )?;
    let declarations = area::read_declarations(paths)?;
    let planner = Planner {
        paths,
        dirs,
        facts,
        mounts,
        observer,
        record: &record,
        device: verified.identity().device,
        now_ms,
        wall,
        lock_wait: Duration::ZERO,
        deadline: started + wall,
        stopped: None,
        truncated: false,
        examined: 0,
        unreached: 0,
        candidates: BTreeMap::new(),
        candidate_total: 0,
        kept: Vec::new(),
        kept_total: 0,
        participation: Vec::new(),
    };
    let mut prunable = if report.truncated { None } else { Some(0u64) };
    for row in &report.plan.candidates.rows {
        prunable = prunable.and_then(|n| n.checked_add(row.bytes));
    }
    let mut held = if report.truncated { None } else { Some(0u64) };
    for row in &report.kept.rows {
        let Some(path) = &row.path else {
            held = None;
            continue;
        };
        match planner.observe_entry(&row.name, path) {
            Ok(observed) if observed.handle.identity().device == planner.device => {
                held = held.and_then(|n| {
                    planner
                        .scan_bytes(&observed.handle)
                        .ok()
                        .and_then(|bytes| n.checked_add(bytes))
                });
            }
            _ => held = None,
        }
    }
    // A quarantined pending root is deliberately omitted from the candidate
    // planner. Count the remaining captured object from its recorded identity.
    let intents = state::read_intents(paths, crate::contract::INTENT_ROWS)?;
    for intent in intents
        .pending()
        .filter(|intent| &intent.volume_uuid == uuid)
    {
        let Some(mount) = record.mount_hint.as_ref() else {
            held = None;
            continue;
        };
        let parent = mount
            .join(&intent.root_dir)
            .join(&intent.folder)
            .join(&intent.unit.parent);
        let captured = parent.join(intent.capture_name());
        let original = parent.join(&intent.unit.name);
        let path = match dirs.path_facts(&captured) {
            Ok(Some(_)) => captured,
            Ok(None) => original,
            Err(_) => {
                held = None;
                continue;
            }
        };
        if report
            .kept
            .rows
            .iter()
            .any(|row| row.path.as_ref() == Some(&path))
        {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            held = None;
            continue;
        };
        match planner.observe_entry(name, &path) {
            Ok(observed)
                if observed.handle.identity().device == planner.device
                    && observed.handle.identity().inode == intent.unit.inode =>
            {
                held = held.and_then(|n| {
                    planner
                        .scan_bytes(&observed.handle)
                        .ok()
                        .and_then(|bytes| n.checked_add(bytes))
                });
            }
            _ => held = None,
        }
    }
    let mut excluded = if declarations.truncated {
        None
    } else {
        Some(0u64)
    };
    for doc in &declarations.documents {
        if doc.reserve != ReserveParticipation::Excluded {
            continue;
        }
        let resolved = match area::resolve(
            dirs,
            facts,
            mounts,
            observer,
            &record,
            &doc.declaration(),
            Some(doc),
        ) {
            Ok(value) => value,
            Err(_) => {
                excluded = None;
                continue;
            }
        };
        let listing = match area::list_names(dirs, &resolved, ROOT_ROWS) {
            Ok(value) => value,
            Err(_) => {
                excluded = None;
                continue;
            }
        };
        if listing.truncated {
            excluded = None;
        }
        for row in listing.names {
            match planner.observe_entry(&row.name, &row.path) {
                Ok(observed)
                    if matches!(observed.marker, MarkerState::Owned(_))
                        && observed.handle.identity().device == planner.device =>
                {
                    excluded = excluded.and_then(|n| {
                        planner
                            .scan_bytes(&observed.handle)
                            .ok()
                            .and_then(|bytes| n.checked_add(bytes))
                    });
                }
                _ => excluded = None,
            }
        }
    }
    // Cache placements do not participate, but verified owned roots are counted.
    match area::resolve_feature(
        dirs,
        facts,
        mounts,
        observer,
        &record,
        area::Feature::Caches,
    ) {
        Ok(folder) => match area::open_existing_feature(dirs, &folder) {
            Ok(Some(handle)) => match dirs.list_names(&handle, ROOT_ROWS) {
                Ok(names) => {
                    if names.truncated {
                        excluded = None;
                    }
                    for name in names.names {
                        match planner.observe_entry(&name, &folder.path.join(&name)) {
                            Ok(observed)
                                if matches!(observed.marker, MarkerState::Owned(_))
                                    && observed.handle.identity().device == planner.device =>
                            {
                                excluded = excluded.and_then(|n| {
                                    planner
                                        .scan_bytes(&observed.handle)
                                        .ok()
                                        .and_then(|bytes| n.checked_add(bytes))
                                });
                            }
                            _ => excluded = None,
                        }
                    }
                }
                Err(_) => excluded = None,
            },
            Ok(None) => {}
            Err(_) => excluded = None,
        },
        Err(_) => excluded = None,
    }
    let kept = report
        .kept
        .rows
        .iter()
        .map(|row| GuardObservedKept {
            area: row.area.clone(),
            name: row.name.clone(),
            root_id: row.root_id.clone(),
            reason: row.reason.clone(),
        })
        .collect();
    Ok(GuardAttribution {
        prunable,
        held,
        excluded,
        kept,
        kept_total: report.kept.total,
    })
}

/// The plan's canonical bytes: one compact JSON document, exactly the bytes the
/// digest covers and a real run must present.
pub fn canonical_plan_bytes(plan: &SpacePrunePlan) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(plan).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the {SPACE_PRUNE_PLAN_SCHEMA} plan: {error}"),
        )
    })?;
    if bytes.len() > DOCUMENT_SIZE_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the {SPACE_PRUNE_PLAN_SCHEMA} plan is {} bytes, over the {DOCUMENT_SIZE_CAP_BYTES}-byte document cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", DOCUMENT_SIZE_CAP_BYTES.into()));
    }
    Ok(bytes)
}

/// Read one `barge.space.prune.plan/1` from its own bytes, refusing an unknown
/// major, a body that is not this major's field set, a list whose counts
/// disagree with its rows, and a candidate whose identity digest does not
/// describe its own device, inode and marker digest.
pub fn read_plan(bytes: &[u8]) -> Result<SpacePrunePlan> {
    let document = crate::contract::parse_document(bytes)?;
    let found = document
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                "the plan has no schema string, so it is not a space-prune plan",
            )
        })?;
    if found != SPACE_PRUNE_PLAN_SCHEMA {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{found}` is an unknown major; this reader knows {SPACE_PRUNE_PLAN_SCHEMA}"),
        )
        .with_detail("found", found.into())
        .with_detail("supported", SPACE_PRUNE_PLAN_SCHEMA.into()));
    }
    let plan: SpacePrunePlan = serde_json::from_value(document).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "the plan does not provide the field set {SPACE_PRUNE_PLAN_SCHEMA} requires: {error}"
            ),
        )
    })?;
    plan.candidates.validate()?;
    if VolumeUuid::parse(plan.volume_uuid.as_str())
        .map_or(true, |parsed| parsed.as_str() != plan.volume_uuid.as_str())
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the plan's volume uuid is not canonical",
        ));
    }
    if plan.policy.reserve_bytes == 0
        || plan.policy.floor_bytes == 0
        || plan.policy.declared_by.trim().is_empty()
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the plan's space policy has a zero threshold or no declarer, so it cannot authorize anything",
        ));
    }
    if plan.target_bytes != plan.policy.reserve_bytes.max(plan.policy.floor_bytes) {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the plan's target does not equal max(reserve_bytes, floor_bytes)",
        ));
    }
    for row in &plan.candidates.rows {
        let invalid = |reason: &str| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the plan's candidate `{}` is not usable: {reason}",
                    row.root_id
                ),
            )
        };
        if !state::is_identifier(&row.root_id) {
            return Err(invalid("its root id is not 32 hex digits"));
        }
        if !is_safe_component(&row.name) {
            return Err(invalid(
                "its name is not a single safe directory component, so a real run could not address it",
            ));
        }
        if !unit::is_sha256(&row.marker_digest) {
            return Err(invalid("its marker digest is not a SHA-256"));
        }
        if row.last_release_ms == 0 {
            return Err(invalid("it records no release, so it is not idle"));
        }
        if row.identity_digest != identity_digest(row.device, row.inode, &row.marker_digest) {
            return Err(invalid(
                "its identity digest does not describe its device, inode and marker digest",
            ));
        }
    }
    Ok(plan)
}

/// Whether one candidate name is a single safe directory component.
///
/// It is deliberately not a root id: an adopted root keeps the directory name it
/// already had, which need not be 32 hex digits, and the plan must accept it.
/// What a real run needs is a name it can address inside its folder, so the name
/// must be non-empty, not `.` or `..`, and contain no separator or NUL.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// The identity a real run re-verifies: device, inode and marker digest.
fn identity_digest(device: u64, inode: u64, marker_digest: &str) -> String {
    sha256_hex(format!("{device}:{inode}:{marker_digest}").as_bytes())
}

/// The measurement's state against the declared policy.
fn state_name(measurement: &SpaceMeasurement, policy: &registry::SpacePolicy) -> &'static str {
    if measurement.free_bytes <= policy.floor_bytes {
        "below_floor"
    } else if measurement.free_bytes <= policy.reserve_bytes {
        "below_reserve"
    } else {
        "above_reserve"
    }
}

/// A durability's name, as a report spells it.
const fn durability_name(durability: registry::Durability) -> &'static str {
    match durability {
        registry::Durability::Expendable => "expendable",
        registry::Durability::Kept => "kept",
        registry::Durability::Undeclared => "undeclared",
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

/// One candidate with the key that orders it.
struct Candidate {
    key: LruKey,
    row: SpaceCandidateRow,
}

/// Why one entry's evidence could not be observed under its own lock.
enum EvidenceError {
    /// The entry's own lock is held past this plan's wait, so nothing about the
    /// entry was read and it is kept as a busy row.
    Locked(String),
    /// The entry could not be opened, or its marker would not settle, so nothing
    /// about it is reported as a candidate.
    Unreadable(String),
}

/// One entry's facts, read while its own shared lock or locks were held.
///
/// The locks stay alive inside this value until the decision is made, so the
/// marker, its digest, the root id, the holds, the activity record and the bytes
/// scan all describe one moment that no consumer's hold and no delete could
/// interleave with.
struct ObservedEntry {
    handle: DirHandle,
    /// The path that was opened, for the row.
    path: PathBuf,
    marker: MarkerState,
    marker_digest: Option<String>,
    root_id: Option<String>,
    /// The lock a hold or a delete of this entry takes: the root lock when the
    /// name is the root id, else the deletion unit's own key.
    _name_lock: Option<StateLock>,
    /// The marker's own root-id lock, when an adopted entry's name differs from
    /// its root id. A hold is keyed by the root id, so both locks are needed.
    _root_id_lock: Option<StateLock>,
}

/// The root id a marker state proves, when it proves one.
fn marker_root_id(marker: &MarkerState) -> Option<String> {
    match marker {
        MarkerState::Owned(marker) => Some(marker.root_id.clone()),
        MarkerState::Absent | MarkerState::Unknown { .. } => None,
    }
}

/// Everything one plan reads, and what it has decided so far.
struct Planner<'a> {
    paths: &'a StatePaths,
    dirs: &'a dyn DirOps,
    facts: &'a dyn VolumeFacts,
    mounts: &'a dyn MountRoot,
    observer: &'a dyn ProcessObserver,
    record: &'a VolumeRecord,
    /// The verified volume's device, so an entry on another device is kept.
    device: crate::os::DeviceId,
    now_ms: u64,
    wall: Duration,
    lock_wait: Duration,
    deadline: Instant,
    stopped: Option<String>,
    truncated: bool,
    examined: usize,
    unreached: usize,
    /// The oldest `ROOT_ROWS` candidates by LRU key. It is bounded while it is
    /// built, so a volume with more roots cannot grow the plan in memory, and
    /// `candidate_total` still reports how many there were.
    candidates: BTreeMap<LruKey, SpaceCandidateRow>,
    candidate_total: usize,
    kept: Vec<SpaceKeptRow>,
    kept_total: usize,
    participation: Vec<SpaceParticipationRow>,
}

impl Planner<'_> {
    /// Whether the plan may still read one more object. The first time the wall
    /// clock is reached it records why, so a bounded plan says what it did not
    /// reach rather than presenting a short one as whole.
    fn has_time(&mut self) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        if Instant::now() < self.deadline {
            return true;
        }
        self.stopped = Some(format!(
            "the plan's {}-second wall clock stopped it",
            self.wall.as_secs()
        ));
        self.truncated = true;
        false
    }

    /// One shared-lock wait, bounded by the plan's deadline as well as its own
    /// lock budget.
    fn lock_deadline(&self) -> Deadline {
        lock_deadline(self.deadline, self.lock_wait)
    }

    /// Record one candidate, keeping only the oldest `ROOT_ROWS` in delete order
    /// while still counting every candidate the volume held.
    fn add_candidate(&mut self, candidate: Candidate) {
        self.candidate_total = self.candidate_total.saturating_add(1);
        if self.candidates.len() < ROOT_ROWS {
            self.candidates.insert(candidate.key, candidate.row);
            return;
        }
        let Some(oldest_kept) = self.candidates.last_key_value().map(|(key, _)| key.clone()) else {
            return;
        };
        if candidate.key < oldest_kept {
            self.candidates.insert(candidate.key, candidate.row);
            self.candidates.pop_last();
        }
    }

    /// Record one kept row, counting every row while bounding the document's.
    fn keep(&mut self, row: SpaceKeptRow) {
        self.kept_total = self.kept_total.saturating_add(1);
        if self.kept.len() < ROOT_ROWS {
            self.kept.push(row);
        }
    }

    /// One kept row.
    fn kept_row(
        &self,
        label: &str,
        name: &str,
        path: Option<PathBuf>,
        root_id: Option<String>,
        state: &str,
        reason: String,
    ) -> SpaceKeptRow {
        SpaceKeptRow {
            area: label.to_owned(),
            name: name.to_owned(),
            path,
            root_id,
            state: state.to_owned(),
            reason,
        }
    }

    /// Observe one entry under its own shared lock or locks.
    ///
    /// The lock a hold or a delete takes comes first: the root lock when the
    /// entry's name is its Barge root id, else the deletion unit's own key, which
    /// is the lock a delete of that unit takes. The marker is read under it, and
    /// an adopted root whose marker proves another root id takes that root id's
    /// lock too and re-reads the marker, so no fact from before the second lock
    /// is used and a marker whose id changed keeps the root.
    fn observe_entry(
        &self,
        name: &str,
        path: &Path,
    ) -> std::result::Result<ObservedEntry, EvidenceError> {
        let name_lock = {
            let mut locks = LockSet::with_deadline(self.paths.clone(), self.lock_deadline());
            if state::is_identifier(name) {
                locks.observe_root(name)
            } else {
                locks.observe_unit(&state::unit_lock_key(&self.record.volume_uuid, "", name))
            }
        };
        let name_lock = match name_lock {
            Ok(lock) => lock,
            Err(error) => {
                return Err(EvidenceError::Locked(format!(
                    "the entry's own lock could not be observed, so nothing about it was read: {}",
                    error.message()
                )))
            }
        };
        let handle = match self.dirs.open_path(path) {
            Ok(handle) => handle,
            Err(error) => {
                return Err(EvidenceError::Unreadable(format!(
                    "the entry could not be opened under its own lock: {error}"
                )))
            }
        };
        let effective_uid = match self.observer.effective_uid() {
            Ok(uid) => uid,
            Err(error) => {
                return Err(EvidenceError::Unreadable(format!(
                    "this account's uid is unreadable: {error}"
                )))
            }
        };
        let expectation = MarkerExpectation {
            volume_uuid: &self.record.volume_uuid,
            resolved_path: path,
            mount_point: self.record.mount_hint.as_deref(),
            effective_uid,
            now_ms: self.now_ms,
        };
        let (mut marker, mut marker_digest) =
            area::read_marker_with_digest(self.dirs, &handle, &expectation);
        let mut root_id_lock = None;
        let first_id = marker_root_id(&marker);
        if let Some(id) = first_id.as_deref() {
            if id != name && state::is_identifier(id) {
                let mut locks = LockSet::with_deadline(self.paths.clone(), self.lock_deadline());
                match locks.observe_root(id) {
                    Ok(lock) => root_id_lock = lock,
                    Err(error) => {
                        return Err(EvidenceError::Locked(format!(
                            "the root's root-id lock could not be observed, so nothing about it was read: {}",
                            error.message()
                        )))
                    }
                }
                let (again, again_digest) =
                    area::read_marker_with_digest(self.dirs, &handle, &expectation);
                if marker_root_id(&again) != first_id {
                    return Err(EvidenceError::Unreadable(
                        "the root's marker names another root after its root-id lock was taken, so it is kept"
                            .to_owned(),
                    ));
                }
                marker = again;
                marker_digest = again_digest;
            }
        }
        Ok(ObservedEntry {
            root_id: marker_root_id(&marker),
            path: path.to_path_buf(),
            handle,
            marker,
            marker_digest,
            _name_lock: name_lock,
            _root_id_lock: root_id_lock,
        })
    }

    /// A root's whole-tree allocated bytes, from a complete bounded scan of the
    /// handle whose own lock is held. An incomplete scan keeps the root: unknown
    /// bytes are never taken as zero.
    fn scan_bytes(&self, handle: &DirHandle) -> std::result::Result<u64, String> {
        // The scan gets what is left of the plan's wall, never its own fixed
        // five-second allowance: a plan that is nearly out of time must not spend
        // it on one root's subtree.
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let scan = area::scan_activity_within(
            self.dirs,
            handle,
            self.now_ms,
            remaining.min(area::ACTIVITY_WALL),
        );
        if !scan.complete {
            return Err(format!(
                "the root's bytes could not be counted completely: {}",
                scan.reason
                    .unwrap_or_else(|| "the scan did not reach the whole subtree".to_owned())
            ));
        }
        Ok(scan.allocated_bytes)
    }

    /// One declared area. An `excluded` area is never listed; an area that cannot
    /// be resolved or listed is reported and the plan continues, so one
    /// undecidable area never blocks the rest.
    fn area(&mut self, document: &AreaDocument) {
        let label = format!("{}/{}", document.namespace, document.area);
        let declaration = document.declaration();
        if declaration.reserve == ReserveParticipation::Excluded {
            self.participation.push(SpaceParticipationRow {
                area: label,
                reserve: "excluded".to_owned(),
                state: "excluded".to_owned(),
                roots: None,
                candidates: None,
                kept: None,
                reason: Some(
                    "the area's reserve is `excluded`, so its roots never yield under the reserve and are not enumerated"
                        .to_owned(),
                ),
            });
            return;
        }
        if !self.has_time() {
            self.participation.push(SpaceParticipationRow {
                area: label,
                reserve: "counts".to_owned(),
                state: "unreached".to_owned(),
                roots: None,
                candidates: None,
                kept: None,
                reason: self.stopped.clone(),
            });
            return;
        }
        let resolved = match area::resolve(
            self.dirs,
            self.facts,
            self.mounts,
            self.observer,
            self.record,
            &declaration,
            Some(document),
        ) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.participation.push(SpaceParticipationRow {
                    area: label,
                    reserve: "counts".to_owned(),
                    state: "unknown".to_owned(),
                    roots: None,
                    candidates: None,
                    kept: None,
                    reason: Some(format!("the area could not be resolved: {error}")),
                });
                return;
            }
        };
        // A declared area whose folder is not there was not enumerated, so its
        // counts are unknown, not zero: an absent area is not an empty one.
        if !resolved.exists() {
            self.participation.push(SpaceParticipationRow {
                area: label,
                reserve: "counts".to_owned(),
                state: "absent".to_owned(),
                roots: None,
                candidates: None,
                kept: None,
                reason: Some(
                    "the area's folder does not exist on this volume, so nothing was enumerated; an absent area is not an empty one"
                        .to_owned(),
                ),
            });
            return;
        }
        // The enumeration reads the folder's names and nothing else: no child is
        // opened, no marker or hold is read and no fact is stated before a row's
        // own shared lock. Everything that decides a row is read under that lock.
        let listing = match area::list_names(self.dirs, &resolved, ROOT_ROWS) {
            Ok(listing) => listing,
            Err(error) => {
                self.participation.push(SpaceParticipationRow {
                    area: label,
                    reserve: "counts".to_owned(),
                    state: "unknown".to_owned(),
                    roots: None,
                    candidates: None,
                    kept: None,
                    reason: Some(format!("the area could not be listed: {error}")),
                });
                return;
            }
        };
        self.examined = self.examined.saturating_add(listing.examined);
        if listing.truncated {
            self.truncated = true;
        }
        let mut examined = 0u64;
        let mut candidates = 0u64;
        let mut kept = 0u64;
        for name in &listing.names {
            if !self.has_time() {
                self.unreached = self
                    .unreached
                    .saturating_add(listing.names.len().saturating_sub(examined as usize));
                break;
            }
            examined = examined.saturating_add(1);
            let before_candidates = self.candidate_total;
            let before_kept = self.kept_total;
            self.area_entry(&label, name);
            if self.candidate_total > before_candidates {
                candidates = candidates.saturating_add(1);
            } else if self.kept_total > before_kept {
                kept = kept.saturating_add(1);
            }
        }
        self.participation.push(SpaceParticipationRow {
            area: label,
            reserve: "counts".to_owned(),
            state: "participating".to_owned(),
            roots: Some(examined),
            candidates: Some(candidates),
            kept: Some(kept),
            reason: listing
                .truncated
                .then(|| "the area's listing stopped at its row bound".to_owned()),
        });
    }

    /// One entry of a participating area, decided under its own shared lock.
    fn area_entry(&mut self, label: &str, name: &area::RootName) {
        let observed = match self.observe_entry(&name.name, &name.path) {
            Ok(observed) => observed,
            Err(EvidenceError::Locked(reason)) => {
                let row = self.kept_row(
                    label,
                    &name.name,
                    Some(name.path.clone()),
                    None,
                    "locked",
                    reason,
                );
                self.keep(row);
                return;
            }
            Err(EvidenceError::Unreadable(reason)) => {
                let row = self.kept_row(
                    label,
                    &name.name,
                    Some(name.path.clone()),
                    None,
                    "unreadable",
                    reason,
                );
                self.keep(row);
                return;
            }
        };
        self.decide_area(label, &name.name, &observed);
    }

    /// Decide one area root from facts read under its own lock.
    fn decide_area(&mut self, label: &str, name: &str, observed: &ObservedEntry) {
        let path = Some(observed.path());
        if observed.handle.identity().device != self.device {
            let row = self.kept_row(
                label,
                name,
                path,
                observed.root_id.clone(),
                "owned",
                "the root is on another device than the verified volume".to_owned(),
            );
            self.keep(row);
            return;
        }
        if !matches!(observed.marker, MarkerState::Owned(_)) {
            let row = self.kept_row(
                label,
                name,
                path,
                observed.root_id.clone(),
                marker_state_name(observed.marker.name()),
                "marker_unverified".to_owned(),
            );
            self.keep(row);
            return;
        }
        let Some(owned) = observed.owned() else {
            return;
        };
        let Some(root_id) = observed.root_id.clone() else {
            let row = self.kept_row(
                label,
                name,
                path,
                None,
                "owned",
                "the root's marker proves no root id".to_owned(),
            );
            self.keep(row);
            return;
        };
        let Some(marker_digest) = observed.marker_digest.clone() else {
            let row = self.kept_row(
                label,
                name,
                path,
                Some(root_id),
                "owned",
                "the root's marker digest is unreadable".to_owned(),
            );
            self.keep(row);
            return;
        };
        if owned.kind == RootKind::CacheRoot {
            let row = self.kept_row(
                label,
                name,
                path,
                Some(root_id),
                "owned",
                "a cache placement's root is never pruned for space".to_owned(),
            );
            self.keep(row);
            return;
        }
        let holds = match area::read_holds(self.paths, self.observer, &root_id) {
            Ok(holds) => holds,
            Err(error) => {
                let row = self.kept_row(
                    label,
                    name,
                    path,
                    Some(root_id),
                    "owned",
                    format!("the root's holds are unreadable: {error}"),
                );
                self.keep(row);
                return;
            }
        };
        let activity = match area::read_activity(self.paths, self.observer, &root_id) {
            Ok(activity) => activity,
            Err(error) => {
                let row = self.kept_row(
                    label,
                    name,
                    path,
                    Some(root_id),
                    "owned",
                    format!("the root's activity record is unreadable: {error}"),
                );
                self.keep(row);
                return;
            }
        };
        let idle = area::assess_idleness(&area::IdleInputs {
            marker: &observed.marker,
            root_id: &root_id,
            volume_uuid: self.record.volume_uuid.as_str(),
            activity: &activity,
            holds: &holds,
            participation: IdleParticipation::Counts,
            now_ms: self.now_ms,
        });
        if !idle.is_idle() {
            let reason = idle.reason.map_or_else(
                || "the root is not idle under the pressure matrix".to_owned(),
                |reason| reason.as_str().to_owned(),
            );
            let row = self.kept_row(label, name, path, Some(root_id), "owned", reason);
            self.keep(row);
            return;
        }
        let bytes = match self.scan_bytes(&observed.handle) {
            Ok(bytes) => bytes,
            Err(reason) => {
                let row = self.kept_row(label, name, path, Some(root_id), "owned", reason);
                self.keep(row);
                return;
            }
        };
        let Some(record) = activity.record.as_ref() else {
            let row = self.kept_row(
                label,
                name,
                path,
                Some(root_id),
                "owned",
                "the activity record is unreadable".to_owned(),
            );
            self.keep(row);
            return;
        };
        let device = observed.handle.identity().device.get();
        let inode = observed.handle.identity().inode;
        self.add_candidate(Candidate {
            key: LruKey {
                last_release_ms: record.last_release_ms,
                first_created_ms: record.first_created_ms,
                root_id: root_id.clone(),
            },
            row: SpaceCandidateRow {
                root_id,
                area: label.to_owned(),
                name: name.to_owned(),
                path: observed.path(),
                bytes,
                last_release_ms: record.last_release_ms,
                first_created_ms: record.first_created_ms,
                device,
                inode,
                marker_digest: marker_digest.clone(),
                identity_digest: identity_digest(device, inode, &marker_digest),
            },
        });
    }

    /// Barge's own scratch folder. It participates by construction, and a root is
    /// pressure-eligible only when the expiry predicate holds with its age
    /// relaxed to "idle now" and the pressure idleness matrix is idle.
    ///
    /// The sweep that finds the entries is a names and scopes index only: no
    /// fact it read decides a row. Every entry is observed again under its own
    /// shared lock, and a native entry is a candidate only when the sweep's own
    /// expiry predicate also accepted it, so a root it vetoed (a nested marker,
    /// for instance) stays kept even if this plan's narrower check would pass.
    fn scratch(&mut self) {
        let label = format!("{}/{}", scratch::SCRATCH_NAMESPACE, scratch::SCRATCH_AREA);
        if !self.has_time() {
            self.participation.push(SpaceParticipationRow {
                area: label,
                reserve: "by_construction".to_owned(),
                state: "unreached".to_owned(),
                roots: None,
                candidates: None,
                kept: None,
                reason: self.stopped.clone(),
            });
            return;
        }
        let folder = match area::resolve_feature(
            self.dirs,
            self.facts,
            self.mounts,
            self.observer,
            self.record,
            area::Feature::Scratch,
        ) {
            Ok(folder) => folder,
            Err(error) => {
                self.participation.push(SpaceParticipationRow {
                    area: label,
                    reserve: "by_construction".to_owned(),
                    state: "unknown".to_owned(),
                    roots: None,
                    candidates: None,
                    kept: None,
                    reason: Some(format!("the scratch folder could not be resolved: {error}")),
                });
                return;
            }
        };
        if !folder.exists() {
            self.participation.push(SpaceParticipationRow {
                area: label,
                reserve: "by_construction".to_owned(),
                state: "absent".to_owned(),
                roots: None,
                candidates: None,
                kept: None,
                reason: Some(
                    "the volume has no scratch folder, so there is no scratch data".to_owned(),
                ),
            });
            return;
        }
        // The sweep is a nested walk too, so it gets the remaining wall rather
        // than a fresh copy of the plan's full window.
        let bounds = SweepBounds {
            examine: scratch::SWEEP_EXAMINE,
            timeout: self.deadline.saturating_duration_since(Instant::now()),
        };
        let plan = match scratch::pressure_plan(
            self.paths,
            self.dirs,
            self.facts,
            self.mounts,
            self.observer,
            &self.record.volume_uuid,
            self.now_ms,
            bounds,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.participation.push(SpaceParticipationRow {
                    area: label,
                    reserve: "by_construction".to_owned(),
                    state: "unknown".to_owned(),
                    roots: None,
                    candidates: None,
                    kept: None,
                    reason: Some(format!("the scratch folder could not be planned: {error}")),
                });
                return;
            }
        };
        self.examined = self.examined.saturating_add(plan.examined);
        if plan.truncated {
            self.truncated = true;
        }
        let mut entries = Vec::new();
        for row in &plan.candidates.rows {
            entries.push(ScratchEntry {
                name: row.name.clone(),
                path: Some(row.path.clone()),
                state: "native".to_owned(),
                reason: None,
            });
        }
        for row in &plan.kept.rows {
            entries.push(ScratchEntry {
                name: row.name.clone(),
                path: row.path.clone(),
                state: row.state.clone(),
                reason: Some(row.reason.clone()),
            });
        }
        let mut examined = 0u64;
        let mut candidates = 0u64;
        let mut kept = 0u64;
        for entry in &entries {
            if !self.has_time() {
                self.unreached = self
                    .unreached
                    .saturating_add(entries.len().saturating_sub(examined as usize));
                break;
            }
            examined = examined.saturating_add(1);
            let before_candidates = self.candidate_total;
            let before_kept = self.kept_total;
            self.scratch_entry(&label, entry);
            if self.candidate_total > before_candidates {
                candidates = candidates.saturating_add(1);
            } else if self.kept_total > before_kept {
                kept = kept.saturating_add(1);
            }
        }
        self.participation.push(SpaceParticipationRow {
            area: label,
            reserve: "by_construction".to_owned(),
            state: "participating".to_owned(),
            roots: Some(examined),
            candidates: Some(candidates),
            kept: Some(kept),
            reason: plan
                .truncated
                .then(|| "the scratch sweep stopped at its bound".to_owned()),
        });
    }

    /// One scratch entry, decided under its own shared lock.
    fn scratch_entry(&mut self, label: &str, entry: &ScratchEntry) {
        let Some(path) = entry.path.clone() else {
            let reason = entry.reason.clone().unwrap_or_else(|| {
                "the entry could not be located, so nothing about it was read".to_owned()
            });
            let row = self.kept_row(label, &entry.name, None, None, &entry.state, reason);
            self.keep(row);
            return;
        };
        let observed = match self.observe_entry(&entry.name, &path) {
            Ok(observed) => observed,
            Err(EvidenceError::Locked(reason)) => {
                let row = self.kept_row(label, &entry.name, Some(path), None, "locked", reason);
                self.keep(row);
                return;
            }
            Err(EvidenceError::Unreadable(reason)) => {
                let reason = entry.reason.clone().unwrap_or(reason);
                let row = self.kept_row(label, &entry.name, Some(path), None, &entry.state, reason);
                self.keep(row);
                return;
            }
        };
        if observed.handle.identity().device != self.device {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                observed.root_id.clone(),
                "owned",
                "the root is on another device than the verified volume".to_owned(),
            );
            self.keep(row);
            return;
        }
        let Some(owned) = observed.owned() else {
            let (state, reason) = match self.scratch_family(&observed.handle) {
                Ok(family) => (
                    family.to_owned(),
                    "the entry is not Barge-owned data, so it never yields under the reserve"
                        .to_owned(),
                ),
                Err(reason) => (entry.state.clone(), reason),
            };
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                observed.root_id.clone(),
                &state,
                reason,
            );
            self.keep(row);
            return;
        };
        if owned.kind != RootKind::ScratchRun {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                observed.root_id.clone(),
                "owned",
                "the owned root is not a scratch run".to_owned(),
            );
            self.keep(row);
            return;
        }
        let Some(root_id) = observed.root_id.clone() else {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                None,
                "owned",
                "the root's marker proves no root id".to_owned(),
            );
            self.keep(row);
            return;
        };
        let Some(marker_digest) = observed.marker_digest.clone() else {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                Some(root_id),
                "owned",
                "the root's marker digest is unreadable".to_owned(),
            );
            self.keep(row);
            return;
        };
        let holds = match area::read_holds(self.paths, self.observer, &root_id) {
            Ok(holds) => holds,
            Err(error) => {
                let row = self.kept_row(
                    label,
                    &entry.name,
                    Some(path),
                    Some(root_id),
                    "owned",
                    format!("the root's holds are unreadable: {error}"),
                );
                self.keep(row);
                return;
            }
        };
        let activity = match area::read_activity(self.paths, self.observer, &root_id) {
            Ok(activity) => activity,
            Err(error) => {
                let row = self.kept_row(
                    label,
                    &entry.name,
                    Some(path),
                    Some(root_id),
                    "owned",
                    format!("the root's activity record is unreadable: {error}"),
                );
                self.keep(row);
                return;
            }
        };
        let idle = area::assess_idleness(&area::IdleInputs {
            marker: &observed.marker,
            root_id: &root_id,
            volume_uuid: self.record.volume_uuid.as_str(),
            activity: &activity,
            holds: &holds,
            participation: IdleParticipation::Scratch,
            now_ms: self.now_ms,
        });
        if !idle.is_idle() {
            let reason = idle.reason.map_or_else(
                || "the root is not idle under the pressure matrix".to_owned(),
                |reason| reason.as_str().to_owned(),
            );
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                Some(root_id),
                "owned",
                reason,
            );
            self.keep(row);
            return;
        }
        // The sweep ran before this root's lock, so its eligibility is only a
        // hint: the expiry predicate, its conclusion facts and its nested-marker
        // walk are all re-evaluated here, under the lock, and a marker that
        // appeared in the tree in between keeps the root.
        let eligibility = match scratch::pressure_eligible(
            self.dirs,
            self.observer,
            self.paths,
            self.record,
            &entry.name,
            &path,
            &observed.handle,
            self.device,
            self.now_ms,
            self.deadline.saturating_duration_since(Instant::now()),
        ) {
            Ok(eligibility) => eligibility,
            Err(error) => {
                let row = self.kept_row(
                    label,
                    &entry.name,
                    Some(path),
                    Some(root_id),
                    "owned",
                    format!("the root's expiry predicate could not be re-checked: {error}"),
                );
                self.keep(row);
                return;
            }
        };
        if !eligibility.eligible {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                Some(root_id),
                "owned",
                eligibility.reason,
            );
            self.keep(row);
            return;
        }
        if eligibility.marker_digest.as_deref() != Some(marker_digest.as_str()) {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                Some(root_id),
                "owned",
                "the root's marker changed while the expiry predicate was re-checked".to_owned(),
            );
            self.keep(row);
            return;
        }
        let bytes = match self.scan_bytes(&observed.handle) {
            Ok(bytes) => bytes,
            Err(reason) => {
                let row = self.kept_row(
                    label,
                    &entry.name,
                    Some(path),
                    Some(root_id),
                    "owned",
                    reason,
                );
                self.keep(row);
                return;
            }
        };
        let Some(record) = activity.record.as_ref() else {
            let row = self.kept_row(
                label,
                &entry.name,
                Some(path),
                Some(root_id),
                "owned",
                "the activity record is unreadable".to_owned(),
            );
            self.keep(row);
            return;
        };
        let device = observed.handle.identity().device.get();
        let inode = observed.handle.identity().inode;
        self.add_candidate(Candidate {
            key: LruKey {
                last_release_ms: record.last_release_ms,
                first_created_ms: record.first_created_ms,
                root_id: root_id.clone(),
            },
            row: SpaceCandidateRow {
                root_id,
                area: format!("{}/{}", scratch::SCRATCH_NAMESPACE, scratch::SCRATCH_AREA),
                name: entry.name.clone(),
                path,
                bytes,
                last_release_ms: record.last_release_ms,
                first_created_ms: record.first_created_ms,
                device,
                inode,
                marker_digest: marker_digest.clone(),
                identity_digest: identity_digest(device, inode, &marker_digest),
            },
        });
    }

    /// The foreign family one entry carries, read under its own lock.
    fn scratch_family(&self, handle: &DirHandle) -> std::result::Result<&'static str, String> {
        let present = |file: &str| {
            self.dirs.child_facts(handle, file).map_err(|error| {
                format!("the entry's foreign marker `{file}` could not be read: {error}")
            })
        };
        if present(scratch::OXIDE_MARKER_FILE)?.is_some() {
            return Ok("oxide_scratch");
        }
        if present(scratch::EFFIGY_MARKER_FILE)?.is_some() {
            return Ok("effigy_scratch");
        }
        Ok("unmarked")
    }
}

/// One scratch entry as the sweep's index names it.
///
/// The index supplies the name, where it lives and whether the expiry predicate
/// accepted it; every fact that decides the row is read again under its own lock.
struct ScratchEntry {
    name: String,
    path: Option<PathBuf>,
    state: String,
    reason: Option<String>,
}

impl ObservedEntry {
    /// The root directory's path, as it was opened.
    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// The marker, when it verifies as an owned root.
    fn owned(&self) -> Option<&area::RootMarker> {
        match &self.marker {
            MarkerState::Owned(marker) => Some(marker),
            MarkerState::Absent | MarkerState::Unknown { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::os::{DeviceId, VerifiedVolume};
    use crate::testkit::VolumeFixture;

    struct Injected(bool);
    impl SpaceFacts for Injected {
        fn measure_space(&self, volume: &VerifiedVolume) -> Result<SpaceMeasurement> {
            SpaceMeasurement::from_blocks(
                Some(4),
                self.0.then_some(3),
                Some(5),
                Some(10),
                12,
                volume.identity().device,
            )
        }
    }

    struct InjectedFlush(bool);
    impl crate::os::Flush for InjectedFlush {
        fn flush_volume(
            &self,
            _volume: &VerifiedVolume,
            _deadline: std::time::Instant,
        ) -> Result<()> {
            if self.0 {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::UnknownState,
                    "injected flush failure",
                ))
            }
        }
    }

    #[test]
    fn flush_failure_is_exposed_through_the_trait() {
        let fixture = VolumeFixture::new("space-flush");
        let volume = os::verify_volume(
            &fixture.facts(),
            &fixture.mounts(),
            &fixture.root,
            &fixture.record.expected_volume(),
        )
        .expect("verified fixture");
        let flush: &dyn crate::os::Flush = &InjectedFlush(false);
        assert_eq!(
            flush
                .flush_volume(&volume, std::time::Instant::now() + Duration::from_secs(30))
                .expect_err("failure")
                .kind(),
            ErrorKind::UnknownState
        );
        let flush: &dyn crate::os::Flush = &InjectedFlush(true);
        assert!(flush
            .flush_volume(&volume, std::time::Instant::now() + Duration::from_secs(30))
            .is_ok());
    }

    #[test]
    fn registered_volume_is_measured_with_undeclared_policy() {
        let fixture = VolumeFixture::new("space-report");
        fixture.register();
        let result = report(
            &fixture.state,
            &fixture.facts(),
            &fixture.mounts(),
            &Injected(true),
        )
        .expect("report");
        let row = &result.volumes.rows[0];
        assert_eq!(row.state, "policy_undeclared");
        assert_eq!(row.policy, None);
        let m = row.measurement.as_ref().expect("measurement");
        assert_eq!(m.free_bytes, 12);
        assert_eq!(m.purgeable, "unknown");
        assert_eq!(m.device, DeviceId::new(fixture.device.get()));
        let envelope = serde_json::json!({"schema": SPACE_SCHEMA, "command": "space", "ok": true, "data": result});
        let encoded = serde_json::to_vec(&envelope).expect("encode");
        read_document(&encoded).expect("valid report");
        for field in ["areas", "scratch", "caches", "attribution"] {
            assert_eq!(
                envelope["data"]["volumes"]["rows"][0][field],
                serde_json::Value::Null
            );
            let mut missing = envelope.clone();
            missing["data"]["volumes"]["rows"][0]
                .as_object_mut()
                .expect("row")
                .remove(field);
            assert!(read_document(&serde_json::to_vec(&missing).expect("encode")).is_err());
        }
        let mut populated = envelope.clone();
        let row = &mut populated["data"]["volumes"]["rows"][0];
        row["areas"] = serde_json::json!({"rows": [{"namespace":"oxide", "area":"pool", "reserve":"counts", "bytes":1, "idle_roots":2, "held_roots":3, "unknown_roots":4}], "count":1, "total":1, "truncated":false});
        row["scratch"] = serde_json::json!({"bytes":1, "idle_eligible_roots":2, "held_roots":3, "unconcluded_roots":4});
        row["caches"] = serde_json::json!({"bytes":1, "reserve":"excluded"});
        row["attribution"] = serde_json::json!({"owned_bytes":1, "excluded_bytes":2, "held_or_unknown_bytes":3, "other_estimate_bytes":4, "cause":"none"});
        read_document(&serde_json::to_vec(&populated).expect("encode")).expect("populated report");
        populated["data"]["volumes"]["rows"][0]["scratch"]["bytes"] = serde_json::json!("invalid");
        assert!(read_document(&serde_json::to_vec(&populated).expect("encode")).is_err());

        let mut broken = envelope;
        broken["data"]["volumes"]["rows"][0]
            .as_object_mut()
            .expect("row")
            .remove("policy");
        assert!(read_document(&serde_json::to_vec(&broken).expect("encode")).is_err());
        let error = report(
            &fixture.state,
            &fixture.facts(),
            &fixture.mounts(),
            &Injected(false),
        )
        .expect_err("unreadable");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    // ---- Space candidate planning (stage 3, slice T4) ----

    use crate::area::{
        self, AreaDeclaration, ConclusionClosure, ConclusionOutcome, HoldRequest, HoldScope,
        KeptReason, NestedScope, ReserveParticipation, RootKind,
    };
    use crate::os::VolumeUuid;
    use crate::registry::{Durability, SpacePolicy};
    use crate::scratch::{self, AllocRequest, ConcludeRequest};
    use crate::testkit::{FakeObserver, Faults};

    /// The fixture volume with a declared space policy.
    fn policy_record(
        fixture: &VolumeFixture,
        reserve: u64,
        floor: u64,
    ) -> crate::registry::VolumeRecord {
        let mut record = fixture.record.clone();
        record.space = Some(SpacePolicy {
            reserve_bytes: reserve,
            floor_bytes: floor,
            declared_by: "fixture".to_owned(),
            declared_ms: 1,
        });
        fixture.register_record(&record);
        // The writer bumps a published record's generation, so the caller gets the
        // record as it is actually stored.
        crate::registry::read(&fixture.state)
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

    /// Hold and release one root, then place its recorded release at `at`.
    fn release_at(
        fixture: &VolumeFixture,
        observer: &FakeObserver,
        root_id: &str,
        at: u64,
        created: u64,
    ) {
        area::hold(
            &fixture.state,
            observer,
            &HoldRequest {
                root_id: root_id.to_owned(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        area::release(&fixture.state, observer, root_id, "prover").expect("the hold releases");
        let path = fixture
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

    /// A plan clock after the fixture's own file times, so a file written a moment
    /// ago is never read as future-dated by the activity scan.
    fn test_now() -> u64 {
        crate::state::now_ms().saturating_add(60_000)
    }

    fn plan_request(volume: &VolumeUuid, now_ms: u64) -> SpacePruneRequest<'_> {
        plan_request_with(volume, now_ms, SPACE_PLAN_WALL, crate::state::LOCK_WAIT)
    }

    /// A request with an explicit wall and lock budget, so a test can put the
    /// plan's deadline next to a busy lock or a slow nested walk.
    fn plan_request_with<'a>(
        volume: &'a VolumeUuid,
        now_ms: u64,
        wall: std::time::Duration,
        lock_wait: std::time::Duration,
    ) -> SpacePruneRequest<'a> {
        SpacePruneRequest {
            volume_uuid: volume,
            now_ms,
            wall,
            lock_wait,
        }
    }

    /// A request with a short lock wait, so a busy root is refused quickly.
    fn plan_request_with_lock_wait<'a>(
        volume: &'a VolumeUuid,
        now_ms: u64,
        lock_wait: std::time::Duration,
    ) -> SpacePruneRequest<'a> {
        plan_request_with(volume, now_ms, SPACE_PLAN_WALL, lock_wait)
    }

    #[test]
    fn a_plan_orders_candidates_across_areas_and_never_plans_excluded_or_unknown_roots() {
        let fixture = VolumeFixture::new("space-plan");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        let other = declaration("other", "example/other", ReserveParticipation::Counts);
        let excluded = declaration(
            "excluded",
            "example/excluded",
            ReserveParticipation::Excluded,
        );
        // Declared but its folder is never created: it was not enumerated.
        let missing = declaration("missing", "example/missing", ReserveParticipation::Counts);
        for document in [&pool, &other, &excluded, &missing] {
            area::declare(&fixture.state, document, 1).expect("the area declares");
        }

        let (old, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        VolumeFixture::payload(&old.path, "payload.bin", &[1u8; 8192]);
        release_at(&fixture, &observer, &old.root_id, 1_000, 500);
        let (newer, _) = fixture.create_root(&record, &other, RootKind::AreaRoot);
        VolumeFixture::payload(&newer.path, "payload.bin", &[2u8; 4096]);
        release_at(&fixture, &observer, &newer.root_id, 2_000, 1_500);
        let (held, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        area::hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: held.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a live fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let (unreleased, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        let (cache, _) = fixture.create_root(&record, &pool, RootKind::CacheRoot);
        release_at(&fixture, &observer, &cache.root_id, 400, 300);
        let (excluded_root, _) = fixture.create_root(&record, &excluded, RootKind::AreaRoot);
        release_at(&fixture, &observer, &excluded_root.root_id, 100, 50);
        std::fs::create_dir_all(fixture.root.join("example/pool/plain-directory"))
            .expect("the unmarked directory is made");

        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        assert!(report.preview);
        assert_eq!(
            report.state, "below_reserve",
            "free 12 is under a 16 reserve"
        );
        assert_eq!(report.policy_generation, record.generation);
        assert_eq!(report.target_bytes, 16);
        assert_eq!(report.candidates, 2);
        let rows = &report.plan.candidates.rows;
        assert_eq!(rows[0].root_id, old.root_id, "the older release goes first");
        assert_eq!(rows[0].area, "example/pool");
        assert_eq!(
            rows[1].root_id, newer.root_id,
            "the LRU order crosses areas"
        );
        assert_eq!(rows[1].area, "example/other");
        assert!(
            rows[0].bytes >= 8192,
            "the candidate rows carry their bytes"
        );
        assert_eq!(rows[0].last_release_ms, 1_000);
        assert_eq!(rows[0].first_created_ms, 500);
        for row in rows {
            assert_eq!(
                row.identity_digest,
                identity_digest(row.device, row.inode, &row.marker_digest)
            );
        }

        let kept = |root_id: &str| {
            report
                .kept
                .rows
                .iter()
                .find(|row| row.root_id.as_deref() == Some(root_id))
                .unwrap_or_else(|| panic!("root {root_id} is kept with a reason"))
                .reason
                .clone()
        };
        assert_eq!(kept(&held.root_id), "hold_live");
        assert_eq!(kept(&unreleased.root_id), "activity_unreleased");
        assert!(
            kept(&cache.root_id).contains("cache placement"),
            "a cache root is kept for its own reason"
        );
        assert!(
            report
                .kept
                .rows
                .iter()
                .any(|row| row.name == "plain-directory" && row.reason == "marker_unverified"),
            "an unmarked directory is reported, not planned"
        );
        assert!(
            report
                .plan
                .candidates
                .rows
                .iter()
                .all(|row| row.root_id != excluded_root.root_id),
            "an excluded area's root never enters the list"
        );
        let excluded_row = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "example/excluded")
            .expect("the excluded area is reported");
        assert_eq!(excluded_row.state, "excluded");
        assert_eq!(excluded_row.reserve, "excluded");
        assert_eq!(
            excluded_row.roots, None,
            "an unlisted area reports no count"
        );
        let pool_row = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "example/pool")
            .expect("the pool area is reported");
        assert_eq!(pool_row.state, "participating");
        assert_eq!(pool_row.candidates, Some(1));
        let missing_row = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "example/missing")
            .expect("the missing area is reported");
        assert_eq!(
            missing_row.state, "absent",
            "an area whose folder is not there was not enumerated"
        );
        assert_eq!(missing_row.roots, None, "an absent area has no count");
        assert_eq!(missing_row.candidates, None);
        assert_eq!(missing_row.kept, None);
        assert!(missing_row
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("does not exist")));

        // The plan is canonical, its digest covers exactly its bytes, and the
        // reader accepts it and refuses a candidate that does not describe itself.
        let bytes = canonical_plan_bytes(&report.plan).expect("the plan encodes");
        assert_eq!(sha256_hex(&bytes), report.plan_sha256);
        let read = read_plan(&bytes).expect("the plan reads back");
        assert_eq!(read, report.plan);
        let mut tampered = read;
        tampered.candidates.rows[0].identity_digest = "0".repeat(64);
        let tampered = serde_json::to_vec(&tampered).expect("the tampered plan encodes");
        assert_eq!(
            read_plan(&tampered)
                .expect_err("a forged identity is refused")
                .kind(),
            ErrorKind::UnknownState
        );
        let mut no_release = report.plan.clone();
        no_release.candidates.rows[0].last_release_ms = 0;
        let no_release = serde_json::to_vec(&no_release).expect("the plan encodes");
        assert!(read_plan(&no_release).is_err());
        let mut no_policy = report.plan.clone();
        no_policy.policy.reserve_bytes = 0;
        let no_policy = serde_json::to_vec(&no_policy).expect("the plan encodes");
        assert!(read_plan(&no_policy).is_err());
        let mut wrong_target = report.plan.clone();
        wrong_target.target_bytes = wrong_target.target_bytes.saturating_add(1);
        let wrong_target = serde_json::to_vec(&wrong_target).expect("the plan encodes");
        assert!(read_plan(&wrong_target).is_err());
        let unknown_major = serde_json::to_vec(&serde_json::json!({
            "schema": "barge.space.prune.plan/2"
        }))
        .expect("the document encodes");
        assert!(read_plan(&unknown_major).is_err());
    }

    #[test]
    fn a_plan_refuses_an_undeclared_policy_a_kept_volume_and_an_absent_volume() {
        let fixture = VolumeFixture::new("space-plan-gates");
        fixture.register();
        let declaration = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &declaration, 1).expect("the area declares");
        let plan_on = |facts: &crate::testkit::FakeVolume| {
            plan(
                &fixture.state,
                &fixture.platform(),
                facts,
                &fixture.mounts(),
                &fixture.observer(),
                &Injected(true),
                &plan_request(&fixture.uuid, test_now()),
            )
        };
        // Undeclared: no policy, never treated as unlimited.
        let facts = fixture.facts();
        let error = plan_on(&facts).expect_err("an absent policy refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            error.detail().get("space_undeclared"),
            Some(&serde_json::Value::Bool(true))
        );

        // Kept: expendable work is never planned on a volume Barge may not expend.
        let mut kept = fixture.record.clone();
        kept.durability = Durability::Kept;
        kept.generation = 2;
        fixture.register_record(&kept);
        let error = plan_on(&fixture.facts()).expect_err("a kept volume refuses");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // Undeclared durability is unknown, not expendable by default.
        let mut undeclared = fixture.record.clone();
        undeclared.durability = Durability::Undeclared;
        undeclared.generation = 3;
        fixture.register_record(&undeclared);
        let error = plan_on(&fixture.facts()).expect_err("undeclared durability refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        // Absent: named, and nothing measured or created.
        let mut expendable = fixture.record.clone();
        expendable.durability = Durability::Expendable;
        expendable.generation = 4;
        expendable.space = Some(SpacePolicy {
            reserve_bytes: 16,
            floor_bytes: 4,
            declared_by: "fixture".to_owned(),
            declared_ms: 1,
        });
        fixture.register_record(&expendable);
        let facts = fixture.facts();
        facts.present.set(false);
        let error = plan_on(&facts).expect_err("an absent volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
    }

    #[test]
    fn a_root_whose_bytes_cannot_be_counted_is_kept_and_the_other_roots_still_plan() {
        let fixture = VolumeFixture::new("space-plan-unknown-bytes");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &pool, 1).expect("the area declares");
        let (first, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &first.root_id, 1_000, 500);
        let (second, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &second.root_id, 2_000, 1_500);

        // The platform reports no modification time, so the bounded scan cannot
        // finish and the root's bytes are unknown: unknown is never zero bytes,
        // and the second root still plans.
        let faults = Faults::new();
        faults.hiding_modified_times();
        let report = plan(
            &fixture.state,
            &faults,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        assert!(report.plan.candidates.rows.is_empty());
        assert_eq!(report.kept.count, 2);
        for row in &report.kept.rows {
            assert!(
                row.reason.contains("bytes could not be counted"),
                "a root with unknown bytes is kept for that reason: {}",
                row.reason
            );
        }
    }

    #[test]
    fn a_failed_area_does_not_stop_the_other_areas_from_planning() {
        let fixture = VolumeFixture::new("space-plan-per-object");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let broken = declaration("pool", "example/pool", ReserveParticipation::Counts);
        let other = declaration("other", "example/other", ReserveParticipation::Counts);
        for document in [&broken, &other] {
            area::declare(&fixture.state, document, 1).expect("the area declares");
        }
        // A regular file where the area folder belongs: the walk refuses that
        // area, and the plan reports it rather than failing the whole volume.
        std::fs::create_dir_all(fixture.root.join("example")).expect("the parent is made");
        std::fs::write(fixture.root.join("example/pool"), b"not a folder")
            .expect("the file is written");
        let (root, _) = fixture.create_root(&record, &other, RootKind::AreaRoot);
        release_at(&fixture, &observer, &root.root_id, 1_000, 500);

        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        assert_eq!(report.plan.candidates.rows.len(), 1);
        assert_eq!(report.plan.candidates.rows[0].root_id, root.root_id);
        let broken = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "example/pool")
            .expect("the broken area is reported");
        assert_eq!(broken.state, "unknown");
        assert!(broken.reason.is_some());
        assert!(broken.roots.is_none(), "an unlisted area reports no count");
        let other = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "example/other")
            .expect("the other area is reported");
        assert_eq!(other.state, "participating");
        assert_eq!(other.candidates, Some(1));
    }

    fn alloc_scratch(
        fixture: &VolumeFixture,
        observer: &FakeObserver,
        project: &str,
    ) -> crate::area::OwnedRoot {
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        scratch::alloc(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            observer,
            &AllocRequest {
                volume_uuid: fixture.uuid.clone(),
                project: project.to_owned(),
                consumer: "prover".to_owned(),
                pid: std::process::id(),
            },
        )
        .expect("the scratch root is allocated")
        .root
    }

    fn conclude_scratch(
        fixture: &VolumeFixture,
        observer: &FakeObserver,
        project: &str,
        root_id: &str,
    ) {
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        scratch::conclude(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            observer,
            &ConcludeRequest {
                volume_uuid: &fixture.uuid,
                root_id,
                holder: "prover",
                project: Some(project),
                outcome: ConclusionOutcome::Closed,
                closure: ConclusionClosure::Positive,
                kept_reason: KeptReason::Evidence,
                nested_scope: NestedScope::Exclusive,
            },
        )
        .expect("the scratch root is concluded");
    }

    #[test]
    fn fallback_report_observation_and_bounds() {
        let fixture = VolumeFixture::under_scratch("fallback-report");
        policy_record(&fixture, 16, 4);
        let path = fixture.dir.join("local-fallback");
        std::fs::create_dir(&path).expect("fixture fallback");
        let mut declaration = declaration("pool", "example/pool", ReserveParticipation::Counts);
        declaration.absent = area::AbsentPolicy::FallbackLocal { path: path.clone() };
        area::declare(&fixture.state, &declaration, 1).expect("declare");
        let facts = fixture.facts();
        let present =
            report(&fixture.state, &facts, &fixture.mounts(), &Injected(true)).expect("present");
        assert_eq!(
            present.fallback_roots.unwrap().rows[0].reason,
            "fallback_while_present"
        );
        facts.present.set(false);
        let absent =
            report(&fixture.state, &facts, &fixture.mounts(), &Injected(true)).expect("absent");
        assert_eq!(
            absent.fallback_roots.unwrap().rows[0].reason,
            "volume_absent"
        );
        std::fs::remove_dir(path).expect("remove fixture fallback");
        let missing =
            report(&fixture.state, &facts, &fixture.mounts(), &Injected(true)).expect("missing");
        assert_eq!(missing.fallback_roots.unwrap().count, 0);
    }

    #[test]
    fn maintain_scratch_only_uses_shared_core() {
        let fixture = VolumeFixture::under_scratch("maintain-space-scratch");
        policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let root = alloc_scratch(&fixture, &observer, "proj");
        VolumeFixture::payload(&root.path, "payload.bin", &[3u8; 8192]);
        conclude_scratch(&fixture, &observer, "proj", &root.root_id);
        area::release(&fixture.state, &observer, &root.root_id, "prover").expect("release");
        let request = crate::prune::MaintainRequest {
            preview: true,
            plan_digest: None,
            plan_bytes: None,
            sweep_bounds: scratch::SweepBounds::default(),
            delete_bounds: crate::delete::DeleteBounds::default(),
            maintain_bounds: crate::prune::MaintainBounds::design(),
            now_ms: test_now(),
        };
        let preview = crate::prune::maintain_with_space(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &InjectedFlush(true),
            &request,
            true,
        )
        .expect("preview");
        assert_eq!(preview.space_triggered, Some(1));
        let bytes = crate::prune::canonical_plan_bytes(&preview.plan).unwrap();
        let real_request = crate::prune::MaintainRequest {
            preview: false,
            plan_digest: Some(&preview.plan_digest),
            plan_bytes: Some(&bytes),
            ..request
        };
        let result = crate::prune::maintain_with_space(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &InjectedFlush(true),
            &real_request,
            true,
        )
        .expect("real");
        assert!(!root.path.exists());
        assert_eq!(
            result.space_rows.unwrap().rows[0]["pruned_root_ids"][0],
            root.root_id
        );
    }

    #[test]
    fn scratch_participates_by_construction_with_only_its_age_relaxed() {
        let fixture = VolumeFixture::new("space-plan-scratch");
        policy_record(&fixture, 16, 4);
        let observer = fixture.observer();

        // Concluded, released and recorded: pressure-eligible now, although the
        // expiry policy's own age has not passed.
        let concluded = alloc_scratch(&fixture, &observer, "proj");
        VolumeFixture::payload(&concluded.path, "payload.bin", &[3u8; 8192]);
        conclude_scratch(&fixture, &observer, "proj", &concluded.root_id);
        area::release(&fixture.state, &observer, &concluded.root_id, "prover")
            .expect("the owner hold releases");
        // Unconcluded but released: never pressure-pruned, whatever its age.
        let unconcluded = alloc_scratch(&fixture, &observer, "proj");
        area::release(&fixture.state, &observer, &unconcluded.root_id, "prover")
            .expect("the owner hold releases");

        // Two entries the plan cannot classify: a symbolic link and a foreign
        // family unit. Both must be reported as kept, neither may vanish, and
        // neither may consume the candidate capacity.
        let scratch_directory = fixture.root.join("scratch");
        std::os::unix::fs::symlink(&fixture.root, scratch_directory.join("links"))
            .expect("the fixture symlink is made");
        let foreign = scratch_directory.join("foreign-run");
        std::fs::create_dir(&foreign).expect("the foreign unit directory is made");
        std::fs::write(foreign.join(".effigy-scratch.json"), b"foreign")
            .expect("the foreign family marker is written");

        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        let rows = &report.plan.candidates.rows;
        assert_eq!(rows.len(), 1, "only the concluded root is eligible");
        assert_eq!(rows[0].root_id, concluded.root_id);
        assert_eq!(rows[0].area, "barge/scratch");
        assert!(rows[0].bytes >= 8192);
        let kept = |root_id: &str| {
            report
                .kept
                .rows
                .iter()
                .find(|row| row.root_id.as_deref() == Some(root_id))
                .unwrap_or_else(|| panic!("root {root_id} is kept with a reason"))
                .reason
                .clone()
        };
        assert!(
            kept(&unconcluded.root_id).contains("unconcluded"),
            "an unconcluded scratch root is kept by the expiry predicate: {}",
            kept(&unconcluded.root_id)
        );
        // The relaxation is the only difference: the expiry sweep's own policy
        // keeps the same concluded root, because its seven-day age has not passed.
        let sweep = scratch::sweep(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &scratch::SweepRequest {
                volume_uuid: &fixture.uuid,
                preview: true,
                plan_digest: None,
                bounds: scratch::SweepBounds::default(),
                delete_bounds: crate::delete::DeleteBounds::default(),
                now_ms: test_now(),
            },
        )
        .expect("the expiry preview is built");
        assert!(
            sweep
                .report
                .plan
                .candidates
                .rows
                .iter()
                .all(|row| row.family != scratch::UnitFamily::Native),
            "the expiry age itself has not passed, so only the pressure predicate may take it"
        );

        let scratch_row = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "barge/scratch")
            .expect("the scratch folder is reported");
        assert_eq!(scratch_row.state, "participating");
        assert_eq!(scratch_row.reserve, "by_construction");
        assert_eq!(scratch_row.candidates, Some(1));
        assert_eq!(
            scratch_row.kept,
            Some(3),
            "unconcluded, symlink and foreign"
        );
        let scratch_kept: Vec<&SpaceKeptRow> = report
            .kept
            .rows
            .iter()
            .filter(|row| row.area == "barge/scratch")
            .collect();
        assert!(
            scratch_kept
                .iter()
                .any(|row| row.name == "links" && row.state == "symlink"),
            "a symbolic link is reported, never followed"
        );
        assert!(
            scratch_kept
                .iter()
                .any(|row| row.name == "foreign-run" && row.state == "effigy_scratch"),
            "a foreign family is reported and never a candidate"
        );
        assert!(
            report
                .plan
                .candidates
                .rows
                .iter()
                .all(|row| row.name != "links" && row.name != "foreign-run"),
            "an unclassified entry never enters the candidate list"
        );
        assert_eq!(report.plan.candidates.rows.len(), report.candidates);
    }

    #[test]
    fn kept_counts_report_every_decision_beyond_the_output_bound() {
        let fixture = VolumeFixture::new("space-plan-kept-bound");
        policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let other = declaration("other", "example/other", ReserveParticipation::Counts);
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        for document in [&other, &pool] {
            area::declare(&fixture.state, document, 1).expect("the area declares");
        }
        let limit = crate::contract::ROOT_ROWS;
        std::fs::create_dir_all(fixture.root.join("example/other"))
            .expect("the other folder is made");
        std::fs::create_dir_all(fixture.root.join("example/pool"))
            .expect("the pool folder is made");
        std::fs::create_dir(fixture.root.join("example/other/only"))
            .expect("the single kept entry is made");
        for index in 0..limit {
            std::fs::create_dir(fixture.root.join(format!("example/pool/d{index:04}")))
                .expect("the unmarked entry is made");
        }

        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        assert_eq!(report.kept.count, limit, "the output is bounded");
        assert_eq!(
            report.kept.total,
            limit + 1,
            "every kept decision is counted, not only the bounded rows"
        );
        assert!(report.truncated);
        assert!(report.truncated_reason.is_some());
        let counts = |area: &str| {
            let row = report
                .participation
                .rows
                .iter()
                .find(|row| row.area == area)
                .expect("the area is reported");
            (row.roots, row.candidates, row.kept)
        };
        assert_eq!(counts("example/other"), (Some(1), Some(0), Some(1)));
        assert_eq!(
            counts("example/pool"),
            (Some(limit as u64), Some(0), Some(limit as u64)),
            "an area's kept count does not depend on the global output bound"
        );
        assert!(report.plan.candidates.rows.is_empty());
    }

    #[test]
    fn a_candidate_name_is_a_safe_component_not_a_root_id() {
        // An adopted root keeps the directory name it already had, so the plan
        // reader validates the name as one safe component, never as a root id.
        for name in ["adopted-cache-run", "my cache", "d0001"] {
            assert!(is_safe_component(name), "{name} is addressable");
        }
        for name in ["", ".", "..", "a/b", "a\0b"] {
            assert!(!is_safe_component(name), "{name} is not one component");
        }
    }

    #[test]
    fn a_busy_root_is_kept_while_the_other_roots_still_plan() {
        let fixture = VolumeFixture::new("space-plan-busy-root");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &pool, 1).expect("the area declares");
        let (busy, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &busy.root_id, 1_000, 500);
        let (free, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &free.root_id, 2_000, 1_500);

        // A delete or a hold takes this root's own lock exclusively. The plan may
        // not read that root's evidence, so it keeps that root alone and the rest
        // of the area still plans.
        let mut locks = LockSet::new(fixture.state.clone());
        let held = locks
            .lock_root(&busy.root_id)
            .expect("the fixture holds the root lock");
        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request_with_lock_wait(
                &fixture.uuid,
                test_now(),
                std::time::Duration::from_millis(30),
            ),
        )
        .expect("the plan is built");
        drop(held);
        assert_eq!(report.plan.candidates.rows.len(), 1);
        assert_eq!(report.plan.candidates.rows[0].root_id, free.root_id);
        let busy_row = report
            .kept
            .rows
            .iter()
            .find(|row| row.name == busy.root_id)
            .expect("the busy root is kept");
        assert_eq!(busy_row.state, "locked");
        assert!(
            busy_row.reason.contains("own lock"),
            "the busy root is kept because its own lock could not be observed: {}",
            busy_row.reason
        );
        let pool_row = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "example/pool")
            .expect("the area is reported");
        assert_eq!(pool_row.candidates, Some(1));
        assert_eq!(pool_row.kept, Some(1));

        // A shared observer holds a compatible lock: the plan takes each root's
        // lock shared, so it never refuses a root another read-only observation
        // is holding, and the root still plans.
        let lock_path = fixture
            .state
            .root_lock_path(&free.root_id)
            .expect("the root lock path");
        let shared = StateLock::shared_if_present(
            &lock_path,
            LockRank::Root,
            Deadline::after(std::time::Duration::from_millis(500)),
        )
        .expect("the shared lock is taken")
        .expect("the root lock file exists");
        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("a shared observer does not block the plan");
        drop(shared);
        assert_eq!(report.plan.candidates.rows.len(), 2);
        assert!(report
            .plan
            .candidates
            .rows
            .iter()
            .any(|row| row.root_id == free.root_id));
    }

    #[test]
    fn a_busy_volume_lock_refuses_the_plan_and_a_shared_one_does_not() {
        let fixture = VolumeFixture::new("space-plan-busy-volume");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &pool, 1).expect("the area declares");
        let (root, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &root.root_id, 1_000, 500);

        let mut locks = LockSet::new(fixture.state.clone());
        let held = locks
            .lock_volume(&fixture.uuid)
            .expect("the fixture holds the volume lock");
        let error = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request_with_lock_wait(
                &fixture.uuid,
                test_now(),
                std::time::Duration::from_millis(30),
            ),
        )
        .expect_err("a busy volume refuses the plan");
        assert_eq!(error.kind(), ErrorKind::Locked);
        assert_eq!(
            error.detail()["lock"],
            fixture
                .state
                .volume_lock_path(&fixture.uuid)
                .display()
                .to_string()
        );
        drop(held);

        // The lock file exists now; a second observer is compatible, so the plan
        // proceeds and still names the root.
        let lock_path = fixture.state.volume_lock_path(&fixture.uuid);
        let shared = StateLock::shared_if_present(
            &lock_path,
            LockRank::Volume,
            Deadline::after(std::time::Duration::from_millis(500)),
        )
        .expect("the shared volume lock is taken")
        .expect("the volume lock file exists");
        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("a shared observer does not block the plan");
        drop(shared);
        assert_eq!(report.plan.candidates.rows.len(), 1);
        assert_eq!(report.plan.candidates.rows[0].root_id, root.root_id);
    }

    #[test]
    fn a_nested_marker_appearing_after_the_sweep_keeps_the_scratch_root() {
        let fixture = VolumeFixture::new("space-plan-scratch-nested-race");
        policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let concluded = alloc_scratch(&fixture, &observer, "proj");
        conclude_scratch(&fixture, &observer, "proj", &concluded.root_id);
        area::release(&fixture.state, &observer, &concluded.root_id, "prover")
            .expect("the owner hold releases");

        // The sweep runs first and accepts the root. The nested marker then appears
        // in the window between that sweep and the locked observation: it is planted
        // when the planner opens the root under its own lock. A plan that trusted
        // the sweep's earlier eligibility would still name the root.
        let faults = Faults::new();
        let run_path = concluded.path.clone();
        let planted = std::cell::Cell::new(false);
        faults.probing_open(move |path| {
            if path == run_path.as_path() && !planted.replace(true) {
                let nested = run_path.join("nested");
                std::fs::create_dir(&nested).expect("the nested directory is made");
                std::fs::write(
                    nested.join(area::ROOT_MARKER_FILE),
                    b"a marker that appeared after the sweep",
                )
                .expect("the nested marker is written");
            }
        });
        let report = plan(
            &fixture.state,
            &faults,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        assert_eq!(
            report.candidates, 0,
            "a nested marker keeps the run even when the sweep accepted it first"
        );
        let row = report
            .kept
            .rows
            .iter()
            .find(|row| row.name == concluded.root_id)
            .expect("the root is kept");
        assert!(
            row.reason.contains("recognised marker"),
            "the nested marker is what keeps it: {}",
            row.reason
        );
    }

    #[test]
    fn the_scratch_recheck_walk_is_bounded_by_the_wall_it_is_given() {
        let fixture = VolumeFixture::new("space-plan-recheck-budget");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let concluded = alloc_scratch(&fixture, &observer, "proj");
        conclude_scratch(&fixture, &observer, "proj", &concluded.root_id);
        area::release(&fixture.state, &observer, &concluded.root_id, "prover")
            .expect("the owner hold releases");
        let platform = fixture.platform();
        let handle = platform
            .open_path(&concluded.path)
            .expect("the concluded root opens");
        let recheck = |wall: std::time::Duration| {
            scratch::pressure_eligible(
                &platform,
                &observer,
                &fixture.state,
                &record,
                &concluded.root_id,
                &concluded.path,
                &handle,
                fixture.device,
                test_now(),
                wall,
            )
            .expect("the recheck runs")
        };
        // A recheck that starts at the deadline gets no room for its nested walk,
        // and a walk that could not finish keeps the root.
        let expired = recheck(std::time::Duration::ZERO);
        assert!(!expired.eligible);
        assert!(
            expired.reason.contains("marker walk"),
            "a wall-bounded walk that could not finish keeps the root: {}",
            expired.reason
        );
        let bounded = recheck(std::time::Duration::from_secs(5));
        assert!(bounded.eligible, "{}", bounded.reason);
        assert!(bounded.marker_digest.is_some());
    }

    #[test]
    fn a_scratch_recheck_walk_uses_the_remaining_plan_wall_not_a_fresh_window() {
        let fixture = VolumeFixture::new("space-plan-recheck-remaining");
        policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let concluded = alloc_scratch(&fixture, &observer, "proj");
        conclude_scratch(&fixture, &observer, "proj", &concluded.root_id);
        area::release(&fixture.state, &observer, &concluded.root_id, "prover")
            .expect("the owner hold releases");

        // The nested directory appears after the sweep, so only the recheck walks
        // it. Its open pauses longer than a fresh one-second sweep window and well
        // inside the plan's three-second wall: a recheck given the remaining wall
        // finishes and plans the root, while a fresh one-second budget would stop
        // the walk and keep it.
        let faults = Faults::new();
        let run_path = concluded.path.clone();
        let planted = std::cell::Cell::new(false);
        faults.probing_open(move |path| {
            if path == run_path.as_path() && !planted.replace(true) {
                std::fs::create_dir(run_path.join("aa-nested"))
                    .expect("the nested directory is made");
            }
        });
        faults.sleeping_child_open_prefix("aa-nested", std::time::Duration::from_millis(1_500));
        let started = Instant::now();
        let report = plan(
            &fixture.state,
            &faults,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request_with(
                &fixture.uuid,
                test_now(),
                std::time::Duration::from_secs(3),
                crate::state::LOCK_WAIT,
            ),
        )
        .expect("the plan is built");
        let elapsed = started.elapsed();
        assert_eq!(
            report.candidates, 1,
            "the recheck's walk budget is the remaining plan wall, not one second"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(1_500),
            "the nested open really paused: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "the walk stayed inside the plan's wall: {elapsed:?}"
        );
    }

    #[test]
    fn a_shared_wait_is_bounded_by_the_plan_wall_as_well_as_the_lock_budget() {
        let past = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("a monotonic instant a second ago");
        assert!(
            lock_deadline(past, std::time::Duration::from_secs(30)).expired(),
            "a wait that starts after the plan deadline is already over"
        );
        let future = Instant::now() + std::time::Duration::from_millis(50);
        let bounded = lock_deadline(future, std::time::Duration::from_secs(30));
        assert!(
            bounded.remaining() <= std::time::Duration::from_millis(50),
            "the wait may not outlast the plan's wall"
        );
        let budget = lock_deadline(
            Instant::now() + std::time::Duration::from_secs(30),
            std::time::Duration::from_millis(20),
        );
        assert!(
            budget.remaining() <= std::time::Duration::from_millis(20),
            "the wait may not outlast its own lock budget either"
        );
    }

    #[test]
    fn a_busy_root_near_the_plan_deadline_does_not_extend_the_wall() {
        let fixture = VolumeFixture::new("space-plan-deadline-lock");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &pool, 1).expect("the area declares");
        let (root, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &root.root_id, 1_000, 500);

        let mut locks = LockSet::new(fixture.state.clone());
        let held = locks
            .lock_root(&root.root_id)
            .expect("the fixture holds the root lock");
        // A huge lock budget must not become the wait: only what is left of the
        // plan's wall bounds it, so the busy root is refused at the deadline.
        let started = Instant::now();
        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request_with(
                &fixture.uuid,
                test_now(),
                std::time::Duration::from_millis(150),
                std::time::Duration::from_secs(30),
            ),
        )
        .expect("the plan is built");
        let elapsed = started.elapsed();
        drop(held);
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "the busy root's wait is bounded by the remaining wall, not by the lock budget: {elapsed:?}"
        );
        assert_eq!(report.candidates, 0);
        let row = report
            .kept
            .rows
            .iter()
            .find(|row| row.name == root.root_id)
            .expect("the busy root is kept");
        assert_eq!(row.state, "locked");
        assert!(row.reason.contains("own lock"), "{}", row.reason);
    }

    #[test]
    fn a_bytes_scan_is_bounded_by_the_plan_remaining_wall() {
        let fixture = VolumeFixture::new("space-plan-deadline-scan");
        let record = policy_record(&fixture, 16, 4);
        let observer = fixture.observer();
        let pool = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &pool, 1).expect("the area declares");
        let (root, _) = fixture.create_root(&record, &pool, RootKind::AreaRoot);
        release_at(&fixture, &observer, &root.root_id, 1_000, 500);

        // The scan's first listing takes far longer than the plan's wall, so a
        // scan that were given its own five-second allowance would finish and
        // plan the root. The remaining budget is what it gets, so the scan is
        // incomplete and the root is kept.
        let faults = Faults::new();
        faults.sleeping_before_children_listing(std::time::Duration::from_millis(1_500));
        let started = Instant::now();
        let report = plan(
            &fixture.state,
            &faults,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &Injected(true),
            &plan_request_with(
                &fixture.uuid,
                test_now(),
                std::time::Duration::from_millis(500),
                crate::state::LOCK_WAIT,
            ),
        )
        .expect("the plan is built");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "the nested scan does not add its own five-second allowance: {elapsed:?}"
        );
        assert_eq!(report.candidates, 0);
        let row = report
            .kept
            .rows
            .iter()
            .find(|row| row.name == root.root_id)
            .expect("the root is kept");
        assert!(
            row.reason.contains("bytes could not be counted"),
            "the scan's own bound is the plan's remaining wall: {}",
            row.reason
        );
    }

    #[test]
    fn an_absent_scratch_folder_is_reported_as_absent_not_as_empty_space() {
        let fixture = VolumeFixture::new("space-plan-scratch-absent");
        policy_record(&fixture, 16, 4);
        let declaration = declaration("pool", "example/pool", ReserveParticipation::Counts);
        area::declare(&fixture.state, &declaration, 1).expect("the area declares");
        let report = plan(
            &fixture.state,
            &fixture.platform(),
            &fixture.facts(),
            &fixture.mounts(),
            &fixture.observer(),
            &Injected(true),
            &plan_request(&fixture.uuid, test_now()),
        )
        .expect("the plan is built");
        let scratch = report
            .participation
            .rows
            .iter()
            .find(|row| row.area == "barge/scratch")
            .expect("the scratch folder is reported");
        assert_eq!(scratch.state, "absent");
        assert!(scratch.roots.is_none());
        assert!(scratch.candidates.is_none());
    }
}
