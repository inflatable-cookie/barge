//! Read-only measurements of registered volumes, and the space-prune candidate
//! plan.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::area::{
    self, AreaDocument, IdleParticipation, LruKey, MarkerExpectation, MarkerStateName,
    ReserveParticipation, RootKind, RootObservation,
};
use crate::contract::{RowSet, AREA_ROWS, DOCUMENT_SIZE_CAP_BYTES, ROOT_ROWS, VOLUME_ROWS};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    self, DirOps, MountRoot, ProcessObserver, SpaceFacts, SpaceMeasurement, VolumeFacts, VolumeUuid,
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

/// Measurements at the time of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceReport {
    /// Time the report completed.
    pub observed_ms: u64,
    /// Registered volumes, bounded by the registry limit.
    pub volumes: RowSet<SpaceRow>,
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

/// What one caller asks the space-prune planner for.
#[derive(Debug, Clone, Copy)]
pub struct SpacePruneRequest<'a> {
    /// The registered volume to plan on.
    pub volume_uuid: &'a VolumeUuid,
    /// The plan's own clock, so a test can place a release in the past.
    pub now_ms: u64,
    /// The plan's wall clock (stage 3 design, "Bounded resources").
    pub wall: Duration,
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
    let record = area::registered_volume(paths, request.volume_uuid)?;
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
    // A preview is a report: it takes the shared volume lock only when the lock
    // file already exists, exactly as `barge space` does, and creates nothing.
    let _volume_lock = StateLock::shared_if_present(
        &paths.volume_lock_path(&record.volume_uuid),
        LockRank::Volume,
        Deadline::after(READ_DEADLINE),
    )?;
    let declarations = area::read_declarations(paths)?;
    let declarations_truncated = declarations.truncated;
    let mut planner = Planner {
        paths,
        dirs,
        facts,
        mounts,
        observer,
        record: &record,
        now_ms: request.now_ms,
        wall: request.wall,
        deadline: Instant::now() + request.wall,
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

/// Everything one plan reads, and what it has decided so far.
struct Planner<'a> {
    paths: &'a StatePaths,
    dirs: &'a dyn DirOps,
    facts: &'a dyn VolumeFacts,
    mounts: &'a dyn MountRoot,
    observer: &'a dyn ProcessObserver,
    record: &'a VolumeRecord,
    now_ms: u64,
    wall: Duration,
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
        // `ActivityMode::Floor` is enough to decide idleness: the pressure matrix
        // needs the activity record, not the last-activity scan, and a candidate's
        // bytes are scanned only after it is otherwise eligible.
        let listing = match area::list_roots_with(
            self.dirs,
            self.observer,
            self.paths,
            &resolved,
            ROOT_ROWS,
            area::ActivityMode::Floor,
        ) {
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
        for root in &listing.roots {
            if !self.has_time() {
                self.unreached = self
                    .unreached
                    .saturating_add(listing.roots.len().saturating_sub(examined as usize));
                break;
            }
            examined = examined.saturating_add(1);
            let before = self.kept_total;
            if self.area_root(&label, root) {
                candidates = candidates.saturating_add(1);
            }
            if self.kept_total > before {
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

    /// Consider one root row of a participating area. Answers whether it became
    /// a candidate; a root that is kept is recorded here and never fails the
    /// plan.
    fn area_root(&mut self, label: &str, root: &RootObservation) -> bool {
        fn kept_row(
            label: &str,
            root: &RootObservation,
            state: &str,
            reason: String,
        ) -> SpaceKeptRow {
            SpaceKeptRow {
                area: label.to_owned(),
                name: root.name.clone(),
                path: Some(root.configured_path.clone()),
                root_id: root.root_id.clone(),
                state: state.to_owned(),
                reason,
            }
        }
        if root.state != MarkerStateName::Owned {
            let reason = root.space_idle.reason.map_or_else(
                || "the entry's marker is not an owned root".to_owned(),
                |reason| reason.as_str().to_owned(),
            );
            let row = kept_row(label, root, marker_state_name(root.state), reason);
            self.keep(row);
            return false;
        }
        if !root.space_idle.is_idle() {
            let reason = root.space_idle.reason.map_or_else(
                || "the root is not idle under the pressure matrix".to_owned(),
                |reason| reason.as_str().to_owned(),
            );
            let row = kept_row(label, root, "owned", reason);
            self.keep(row);
            return false;
        }
        let (Some(root_id), Some(device), Some(inode), Some(marker_digest), Some(kind)) = (
            root.root_id.clone(),
            root.device,
            root.inode,
            root.marker_digest.clone(),
            root.kind,
        ) else {
            let row = kept_row(
                label,
                root,
                "owned",
                "the root's identity or kind is unreadable".to_owned(),
            );
            self.keep(row);
            return false;
        };
        if kind == RootKind::CacheRoot {
            let row = kept_row(
                label,
                root,
                "owned",
                "a cache placement's root is never pruned for space".to_owned(),
            );
            self.keep(row);
            return false;
        }
        let Some(last_release_ms) = root.activity_record.last_release_ms else {
            let row = kept_row(
                label,
                root,
                "owned",
                "the activity record's release time is unreadable".to_owned(),
            );
            self.keep(row);
            return false;
        };
        if last_release_ms == 0 {
            let row = kept_row(
                label,
                root,
                "owned",
                "the activity record records no release".to_owned(),
            );
            self.keep(row);
            return false;
        }
        let first_created_ms = root.activity_record.first_created_ms.unwrap_or(0);
        let bytes = match self.root_bytes(&root.configured_path, device, inode) {
            Ok(bytes) => bytes,
            Err(reason) => {
                let row = kept_row(label, root, "owned", reason);
                self.keep(row);
                return false;
            }
        };
        self.add_candidate(Candidate {
            key: LruKey {
                last_release_ms,
                first_created_ms,
                root_id: root_id.clone(),
            },
            row: SpaceCandidateRow {
                root_id,
                area: label.to_owned(),
                name: root.name.clone(),
                path: root.configured_path.clone(),
                bytes,
                last_release_ms,
                first_created_ms,
                device,
                inode,
                marker_digest: marker_digest.clone(),
                identity_digest: identity_digest(device, inode, &marker_digest),
            },
        });
        true
    }

    /// A root's whole-tree allocated bytes, from a complete bounded scan under
    /// the identity the listing observed. An incomplete scan, a changed identity
    /// or an unreadable root keeps it: unknown bytes are never taken as zero.
    fn root_bytes(&self, path: &Path, device: u64, inode: u64) -> std::result::Result<u64, String> {
        let handle = self
            .dirs
            .open_path(path)
            .map_err(|error| format!("the root cannot be opened to count its bytes: {error}"))?;
        if handle.identity().device.get() != device || handle.identity().inode != inode {
            return Err("the root's identity changed while the plan was built".to_owned());
        }
        let scan = area::scan_activity(self.dirs, &handle, self.now_ms);
        if !scan.complete {
            return Err(format!(
                "the root's bytes could not be counted completely: {}",
                scan.reason
                    .unwrap_or_else(|| "the scan did not reach the whole subtree".to_owned())
            ));
        }
        Ok(scan.allocated_bytes)
    }

    /// Barge's own scratch folder. It participates by construction, and a root is
    /// pressure-eligible only when the expiry predicate holds with its age
    /// relaxed to "idle now" and the pressure idleness matrix is idle.
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
        let bounds = SweepBounds {
            examine: scratch::SWEEP_EXAMINE,
            timeout: self.wall,
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
        let mut examined = 0u64;
        let mut candidates = 0u64;
        let mut kept = 0u64;
        for row in &plan.candidates.rows {
            if !self.has_time() {
                self.unreached = self
                    .unreached
                    .saturating_add(plan.candidates.rows.len().saturating_sub(examined as usize));
                break;
            }
            examined = examined.saturating_add(1);
            match self.scratch_root(row) {
                Ok(candidate) => {
                    self.add_candidate(candidate);
                    candidates = candidates.saturating_add(1);
                }
                Err(reason) => {
                    self.keep(SpaceKeptRow {
                        area: label.clone(),
                        name: row.name.clone(),
                        path: Some(row.path.clone()),
                        root_id: state::is_identifier(&row.name).then(|| row.name.clone()),
                        state: "owned".to_owned(),
                        reason,
                    });
                    kept = kept.saturating_add(1);
                }
            }
        }
        for row in &plan.kept.rows {
            examined = examined.saturating_add(1);
            kept = kept.saturating_add(1);
            self.keep(SpaceKeptRow {
                area: label.clone(),
                name: row.name.clone(),
                path: row.path.clone(),
                root_id: state::is_identifier(&row.name).then(|| row.name.clone()),
                state: row.state.clone(),
                reason: row.reason.clone(),
            });
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

    /// Re-verify one native scratch run and, when the pressure matrix is idle,
    /// count its bytes. A row is only a candidate when its marker still matches,
    /// its evidence belongs to this root and volume, and every input concluded.
    fn scratch_root(&self, row: &scratch::PlanRow) -> std::result::Result<Candidate, String> {
        let handle = self
            .dirs
            .open_path(&row.path)
            .map_err(|error| format!("the root cannot be opened: {error}"))?;
        if handle.identity().device.get() != row.device || handle.identity().inode != row.inode {
            return Err("the root's identity changed while the plan was built".to_owned());
        }
        let expectation = MarkerExpectation {
            volume_uuid: &self.record.volume_uuid,
            resolved_path: &row.path,
            mount_point: self.record.mount_hint.as_deref(),
            effective_uid: self
                .observer
                .effective_uid()
                .map_err(|error| format!("this account's uid is unreadable: {error}"))?,
            now_ms: self.now_ms,
        };
        let (marker, digest) = area::read_marker_with_digest(self.dirs, &handle, &expectation);
        if digest.as_deref() != Some(row.marker_digest.as_str()) {
            return Err("the root's marker changed while the plan was built".to_owned());
        }
        let holds = area::read_holds(self.paths, self.observer, &row.name)
            .map_err(|error| format!("the root's holds are unreadable: {error}"))?;
        let activity = area::read_activity(self.paths, self.observer, &row.name)
            .map_err(|error| format!("the root's activity record is unreadable: {error}"))?;
        let idle = area::assess_idleness(&area::IdleInputs {
            marker: &marker,
            root_id: &row.name,
            volume_uuid: self.record.volume_uuid.as_str(),
            activity: &activity,
            holds: &holds,
            participation: IdleParticipation::Scratch,
            now_ms: self.now_ms,
        });
        if !idle.is_idle() {
            return Err(idle.reason.map_or_else(
                || "the root is not idle under the pressure matrix".to_owned(),
                |reason| reason.as_str().to_owned(),
            ));
        }
        let Some(record) = activity.record.as_ref() else {
            return Err("the root's activity record is unreadable".to_owned());
        };
        if record.last_release_ms == 0 {
            return Err("the activity record records no release".to_owned());
        }
        let scan = area::scan_activity(self.dirs, &handle, self.now_ms);
        if !scan.complete {
            return Err(format!(
                "the root's bytes could not be counted completely: {}",
                scan.reason
                    .unwrap_or_else(|| "the scan did not reach the whole subtree".to_owned())
            ));
        }
        let marker_digest = row.marker_digest.clone();
        Ok(Candidate {
            key: LruKey {
                last_release_ms: record.last_release_ms,
                first_created_ms: record.first_created_ms,
                root_id: row.name.clone(),
            },
            row: SpaceCandidateRow {
                root_id: row.name.clone(),
                area: format!("{}/{}", scratch::SCRATCH_NAMESPACE, scratch::SCRATCH_AREA),
                name: row.name.clone(),
                path: row.path.clone(),
                bytes: scan.allocated_bytes,
                last_release_ms: record.last_release_ms,
                first_created_ms: record.first_created_ms,
                device: row.device,
                inode: row.inode,
                marker_digest: marker_digest.clone(),
                identity_digest: identity_digest(row.device, row.inode, &marker_digest),
            },
        })
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
        fn flush_volume(&self, _volume: &VerifiedVolume) -> Result<()> {
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
            flush.flush_volume(&volume).expect_err("failure").kind(),
            ErrorKind::UnknownState
        );
        let flush: &dyn crate::os::Flush = &InjectedFlush(true);
        assert!(flush.flush_volume(&volume).is_ok());
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
        SpacePruneRequest {
            volume_uuid: volume,
            now_ms,
            wall: SPACE_PLAN_WALL,
        }
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
        for document in [&pool, &other, &excluded] {
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
