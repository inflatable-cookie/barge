//! Writer custody: the evidence Barge can establish, the three verdicts and the
//! deletion authority only `Safe` carries (stage 1 design, "Writer custody and
//! safe deletion").
//!
//! macOS has no unprivileged primitive that proves "no process holds this
//! subtree open", so the verdict is explicitly *cooperative holds plus a bounded
//! same-uid open-descriptor observation*. Custody is decided about one
//! [`DeletionUnit`]: a directory or a file, inside an area folder, with an
//! optional marker of a recognised family, the identity the plan recorded and the
//! grant that authorises its deletion. `Safe` requires all of:
//!
//! - every recorded hold for the unit released or positively stale (Barge holds
//!   only ever name a Barge root id, so a unit no Barge process could have held
//!   has none to read);
//! - the same-uid descriptor scan complete within its bound and empty for the
//!   unit, with the calling process's own descriptors excluded by pid and start
//!   identity (the walk's and the checks' handles are not another writer) and
//!   every process of another effective uid classified and reported rather than
//!   inspected;
//! - every same-uid process whose descriptor listing the kernel answered `EPERM`
//!   for re-observed by kernel identity until it settled on a positive fact — a
//!   later listing, a gone process, a changed start identity or a changed
//!   effective uid — never on elapsed time (barge#012), with that bounded record
//!   carried on every verdict, refusals included;
//! - a vnode descriptor whose path the platform hides counts as an OS blind spot,
//!   counted and reported, only when the process's own code-signing status
//!   verifies it as a platform binary and its start identity is unchanged around
//!   that read (barge#013, Tom's decision e528a712); every other hidden
//!   descriptor is unreadable and refuses;
//! - the unit's device, inode and kind equal to the observation the caller was
//!   given;
//! - its marker (when it records one) still holds exactly the bytes the caller
//!   observed, at its family's own filename, and a unit that records no marker
//!   carries none of the recognised families - a unit that became marked between
//!   the plan and the effect is kept;
//! - the recorded grant is the form the unit's marker family needs - a native root
//!   for Barge's own marker, that family's delegation for a foreign marker, and a
//!   legacy claim for an unmarked unit - and is still recorded on the volume, so
//!   no delete path skips a grant or trusts one revoked since the plan;
//! - the volume still verified; and
//! - no pending intent naming another unit.
//!
//! Two residuals are named in [`capability_report`] rather than implied: a
//! process of another effective uid, which the kernel's own process table
//! classifies and the evidence counts but nothing inspects, and a descriptor
//! whose path a verified platform binary's process hides, which is counted and
//! reported as the OS blind spot because nothing can read where it points. A
//! process that dropped its hold and kept writing is named there too. A wrong
//! delete stays recoverable through the intent sequence and quarantine rename.
//! The scan excludes the calling `barge` process and nothing else: another Barge
//! process, its child or its parent holds a descriptor like any other writer.
//! `lsof` is never custody: it is external, unbounded and its silence is not a
//! kernel guarantee, so nothing here reads it.
//!
//! Deletion and swap take a [`Safe`](CustodyVerdict::Safe) value, so a caller
//! cannot pass a boolean and cannot reach the effect on a refusal.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::area::{self, HoldState, MarkerExpectation, MarkerState};
use crate::contract::{HOLD_ROWS, MARKER_CAP_BYTES};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    BlindSpotEvidence, DescriptorScan, DescriptorScanRequest, DirOps, EntryKind, MountRoot,
    ProcessObserver, RetryEvidence, SelfExclusion, VolumeFacts, VolumeUuid,
};
use crate::registry;
use crate::state::{self, IntentRecord, StatePaths};
use crate::unit::{self, DeletionUnit, GrantKind, MarkerFamily, UnitKind, UnitMarker};

/// The capability this build offers, as `describe.custody.capability` states it.
pub const CAPABILITY: &str = "cooperative_holds_plus_same_uid_descriptor_observation";

/// How many same-uid processes one scan may observe before it is bounded.
pub const SCAN_PROCESSES: usize = 4096;

/// How many descriptors one scan may observe before it is bounded.
pub const SCAN_DESCRIPTORS: usize = 65_536;

/// How long one descriptor scan may take before it is bounded.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// One pending intent, as custody needs to know it: an intent for any other unit
/// means a deletion is already in flight, so this one must not start, and an
/// intent for this unit means it is already mid-delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingIntent {
    /// The intent's id.
    pub intent_id: String,
    /// The parent directory the unit's intent recorded, relative to its area
    /// folder.
    pub parent: String,
    /// The unit name the intent acts on.
    pub name: String,
}

impl PendingIntent {
    /// Project the recorded intents that still need a replay decision.
    pub fn pending(records: &[IntentRecord]) -> Vec<Self> {
        records
            .iter()
            .filter(|record| record.is_pending())
            .map(|record| Self {
                intent_id: record.intent_id.clone(),
                parent: record.unit.parent.clone(),
                name: record.unit.name.clone(),
            })
            .collect()
    }

    /// Whether this pending intent names `unit`.
    pub fn names(&self, unit: &DeletionUnit) -> bool {
        self.parent == unit.parent && self.name == unit.name
    }
}

/// The bounds one custody decision runs within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustodyBounds {
    /// How many hold files one unit may carry.
    pub holds: usize,
    /// How many same-uid processes the scan may observe.
    pub processes: usize,
    /// How many descriptors the scan may observe.
    pub descriptors: usize,
    /// How long the scan may take.
    pub timeout: Duration,
}

impl Default for CustodyBounds {
    fn default() -> Self {
        Self {
            holds: HOLD_ROWS,
            processes: SCAN_PROCESSES,
            descriptors: SCAN_DESCRIPTORS,
            timeout: SCAN_TIMEOUT,
        }
    }
}

/// The evidence a `Safe` verdict carries to the effect it authorises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyEvidence {
    /// The unit the evidence is about, re-read and matched.
    pub unit_name: String,
    /// The unit's kind: `directory` or `file`.
    pub unit_kind: String,
    /// The parent directory the unit was found in, relative to its area folder.
    pub parent: String,
    /// The volume UUID that verified.
    pub volume_uuid: VolumeUuid,
    /// The device the volume reported.
    pub device: u64,
    /// The inode the unit's own facts reported.
    pub inode: u64,
    /// The marker whose bytes matched the observation, when the unit has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<UnitMarker>,
    /// The recorded grant the deletion rests on.
    pub grant: crate::unit::UnitGrant,
    /// How many hold files the unit carried.
    pub holds: usize,
    /// How many of them were stale and therefore released by evidence.
    pub holds_stale: usize,
    /// How many same-uid processes the scan observed.
    pub processes: usize,
    /// How many descriptors the scan observed.
    pub descriptors: usize,
    /// The calling process's own pid. The scan excluded that process's
    /// descriptors, so this is the one process the evidence does not cover and
    /// the only exclusion it made.
    pub self_excluded: u32,
    /// How many processes of another effective uid the scan classified from the
    /// kernel's own process table. They were not inspected at all: this is the
    /// named blind spot, reported so the evidence never implies they were
    /// observed.
    pub foreign_uid_processes: usize,
    /// The bounded record of every same-uid `EPERM` the scan re-observed: how
    /// many there were, and for the first [`crate::os::RETRY_RECORDS`] of them
    /// the command, start identity, errno sequence and resolution, so a native
    /// diagnostic can tell an intermittent answer from a persistent one.
    pub retry: RetryEvidence,
    /// The OS blind spot the scan counted: how many same-uid processes the
    /// platform's own code-signing status verified as platform binaries with
    /// vnode descriptors whose path and vnode facts it hides
    /// (`processes`), how many such descriptors there were in total, and the
    /// record of **each** of those processes — pid, start identity, command and
    /// how many of its descriptors were hidden (barge#013, Tom's decision
    /// e528a712). Nothing can read where those descriptors point, so this is
    /// reported rather than implied, and it never blocks `Safe`. The record is
    /// bounded but never truncated: a scan that meets more than
    /// [`crate::os::BLIND_SPOT_RECORDS`] such processes refuses rather than
    /// count one it cannot name.
    pub os_blind_spot: BlindSpotEvidence,
    /// When the verdict was decided.
    pub checked_ms: u64,
}

/// The three verdicts. Only [`Safe`](Self::Safe) authorises an effect.
///
/// Every verdict carries the bounded record of the scan's `EPERM`
/// re-observation, including the refusals: that is where a native diagnostic
/// needs it, and [`into_evidence`](Self::into_evidence) puts it in the failure's
/// detail so a refusal does not discard it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustodyVerdict {
    /// Every fact Barge can establish agrees: a swap or delete may proceed.
    Safe(Box<CustodyEvidence>),
    /// A holder or an open descriptor was found. Nothing may be deleted.
    Unsafe {
        /// What was found, for a human.
        reason: String,
        /// The bounded record of the scan's `EPERM` re-observation.
        retry: RetryEvidence,
    },
    /// A fact could not be read or could not be completed within its bound.
    /// Unknown is a refusal and is never read as safe.
    Unknown {
        /// Why the verdict cannot be decided.
        reason: String,
        /// The bounded record of the scan's `EPERM` re-observation.
        retry: RetryEvidence,
    },
}

impl CustodyVerdict {
    /// Whether the verdict authorises an effect.
    pub const fn is_safe(&self) -> bool {
        matches!(self, Self::Safe(_))
    }

    /// The verdict's name, as a document reports it.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Safe(_) => "safe",
            Self::Unsafe { .. } => "unsafe",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// The evidence, when the verdict is safe.
    pub fn evidence(&self) -> Option<&CustodyEvidence> {
        match self {
            Self::Safe(evidence) => Some(evidence),
            Self::Unsafe { .. } | Self::Unknown { .. } => None,
        }
    }

    /// Why a non-safe verdict is not safe.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Safe(_) => None,
            Self::Unsafe { reason, .. } | Self::Unknown { reason, .. } => Some(reason),
        }
    }

    /// The bounded record of the scan's `EPERM` re-observation, on every verdict.
    pub fn retry(&self) -> &RetryEvidence {
        match self {
            Self::Safe(evidence) => &evidence.retry,
            Self::Unsafe { retry, .. } | Self::Unknown { retry, .. } => retry,
        }
    }

    /// The authority an effect needs, or the refusal that replaces it.
    ///
    /// This is the type-level gate: [`crate::delete`] takes the evidence a
    /// `Safe` verdict produced and nothing else. A refusal's bounded retry
    /// record travels in the failure's detail rather than being discarded with
    /// the verdict.
    pub fn into_evidence(self) -> Result<CustodyEvidence> {
        match self {
            Self::Safe(evidence) => Ok(*evidence),
            Self::Unsafe { reason, retry } | Self::Unknown { reason, retry } => Err(Error::new(
                ErrorKind::CustodyNotProven,
                format!("custody is not proven: {reason}"),
            )
            .with_detail("retried_unreadable", retry.retried_unreadable.into())
            .with_detail("retried", serde_json::json!(retry.retried))),
        }
    }
}

/// Decide custody for one deletion unit.
///
/// Every fact is read fresh. A unit whose grant does not cover its marker family,
/// a hold that is live or unknown, a scan that is bounded or unreadable, a unit
/// whose identity, kind or marker bytes moved, a unit that became marked, a
/// volume that no longer verifies, and a pending intent for another unit (or for
/// this one) all refuse. Nothing is written and nothing is deleted here.
#[allow(clippy::too_many_arguments)]
pub fn take_custody(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    unit: &DeletionUnit,
    pending: &[PendingIntent],
    bounds: CustodyBounds,
) -> CustodyVerdict {
    match decide(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        unit,
        pending,
        bounds,
    ) {
        Ok(evidence) => CustodyVerdict::Safe(Box::new(evidence)),
        Err(Refusal::Unsafe { reason, retry }) => CustodyVerdict::Unsafe { reason, retry },
        Err(Refusal::Unknown { reason, retry }) => CustodyVerdict::Unknown { reason, retry },
    }
}

/// A non-safe verdict, before it becomes a [`CustodyVerdict`].
///
/// A refusal raised before the scan states no retry evidence; a refusal the scan
/// produced carries its bounded record, which is what keeps that record on the
/// refusal path too.
enum Refusal {
    Unsafe {
        reason: String,
        retry: RetryEvidence,
    },
    Unknown {
        reason: String,
        retry: RetryEvidence,
    },
}

impl Refusal {
    /// A refusal for a fact that is not safe, before any scan was taken.
    fn refuse_unsafe(reason: impl Into<String>) -> Self {
        Self::Unsafe {
            reason: reason.into(),
            retry: RetryEvidence::default(),
        }
    }

    /// An unknown, before any scan was taken. Unknown is a refusal.
    fn refuse_unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
            retry: RetryEvidence::default(),
        }
    }

    /// The same refusal, with the bounded retry record the scan produced.
    fn with_retry(self, retry: RetryEvidence) -> Self {
        match self {
            Self::Unsafe { reason, .. } => Self::Unsafe { reason, retry },
            Self::Unknown { reason, .. } => Self::Unknown { reason, retry },
        }
    }
}

/// Read the holds that could cover `unit`.
///
/// A Barge hold is keyed by a 32-hex-digit root id, and [`area::hold`] refuses any
/// other name, so a unit named otherwise has no Barge hold to read. A unit whose
/// name is a valid area name still has its hold directory read, so a hold that is
/// somehow there is decided rather than ignored; anything else has none.
fn read_unit_holds(
    state_paths: &StatePaths,
    observer: &dyn ProcessObserver,
    unit: &DeletionUnit,
) -> Result<area::HoldReading> {
    if state::is_identifier(&unit.name) {
        return area::read_holds(state_paths, observer, &unit.name);
    }
    if area::validate_name("unit_name", &unit.name).is_ok() {
        return area::read_unit_holds(state_paths, observer, &unit.name);
    }
    Ok(area::HoldReading::empty())
}

#[allow(clippy::too_many_arguments)]
fn decide(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    unit: &DeletionUnit,
    pending: &[PendingIntent],
    bounds: CustodyBounds,
) -> std::result::Result<CustodyEvidence, Refusal> {
    // 0. The unit's own record: its plan digest, parent, name, kind, marker and
    //    grant must each have one well-formed shape, and the grant must be the
    //    form its marker family needs. A record that does not validate is never
    //    acted on.
    unit.validate()
        .map_err(|error| Refusal::refuse_unknown(format!("the unit record is invalid: {error}")))?;

    // 1. Holds: released, or positively stale. Anything else is a refusal.
    let holds = read_unit_holds(state_paths, observer, unit).map_err(|error| {
        Refusal::refuse_unknown(format!("the unit's holds are unreadable: {error}"))
    })?;
    if holds.total > bounds.holds || holds.truncated {
        return Err(Refusal::refuse_unknown(format!(
            "the unit carries {} hold files, over the {}-hold bound",
            holds.total, bounds.holds
        )));
    }
    let mut stale = 0usize;
    for (record, hold_state, reason) in &holds.holds {
        match hold_state {
            HoldState::Live => {
                return Err(Refusal::refuse_unsafe(format!(
                    "the holder `{}` on pid {} is live",
                    record.holder, record.pid
                )))
            }
            HoldState::Unknown => {
                return Err(Refusal::refuse_unknown(format!(
                    "the hold `{}` cannot be decided: {}",
                    record.holder,
                    reason.as_deref().unwrap_or("no reason was recorded")
                )))
            }
            HoldState::Stale => stale = stale.saturating_add(1),
        }
    }

    // 2. No pending intent may name another unit, and this unit must not already
    //    be mid-delete.
    if let Some(intent) = pending.first() {
        if intent.names(unit) {
            return Err(Refusal::refuse_unknown(format!(
                "the pending intent {} already names this unit",
                intent.intent_id
            )));
        }
        return Err(Refusal::refuse_unknown(format!(
            "the pending intent {} names `{}`, so this deletion must wait",
            intent.intent_id, intent.name
        )));
    }

    // 3. The volume still verifies, and the unit is still the entry, the kind and
    //    the marker bytes the caller observed.
    let record = registry::read(state_paths)
        .map_err(|error| Refusal::refuse_unknown(format!("the registry is unreadable: {error}")))?
        .volume(&unit.volume_uuid)
        .cloned()
        .ok_or_else(|| {
            Refusal::refuse_unknown(format!("volume {} is not registered", unit.volume_uuid))
        })?;
    // The recorded grant must still be on this volume: a delegation or claim
    // removed between the plan and the effect keeps the unit (design failure
    // matrix), so the registry the effect re-reads is where it is decided.
    if let Err(reason) = grant_still_recorded(&record, unit) {
        return Err(Refusal::refuse_unknown(reason));
    }
    let opened = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &unit.root_dir,
        &unit.folder,
    )
    .map_err(|error| {
        Refusal::refuse_unknown(format!("the area folder does not verify: {error}"))
    })?;
    let Some((area_path, area_handle)) = opened else {
        return Err(Refusal::refuse_unknown(format!(
            "the area folder for `{}` is not there",
            unit.name
        )));
    };
    let parent = unit
        .open_parent(dirs, &area_handle)
        .map_err(|error| {
            Refusal::refuse_unknown(format!("the unit's parent cannot be opened: {error}"))
        })?
        .ok_or_else(|| {
            Refusal::refuse_unknown(format!(
                "the unit's parent inside the area folder for `{}` is not there",
                unit.name
            ))
        })?;
    let unit_path = unit.path_in(&area_path);
    let Some(observed) = dirs
        .child_facts(&parent, &unit.name)
        .map_err(|error| Refusal::refuse_unknown(format!("the unit cannot be read: {error}")))?
    else {
        return Err(Refusal::refuse_unknown(format!(
            "the unit at {} is not there",
            unit_path.display()
        )));
    };
    if observed.device != unit.device || observed.inode != unit.inode {
        return Err(Refusal::refuse_unknown(format!(
            "the unit at {} is device {} inode {}, not the observed {} / {}",
            unit_path.display(),
            observed.device,
            observed.inode,
            unit.device,
            unit.inode
        )));
    }
    if observed.kind != unit.entry_kind() {
        return Err(Refusal::refuse_unknown(format!(
            "the unit at {} is a {}, not the observed {}",
            unit_path.display(),
            observed.kind.as_str(),
            unit.kind.as_str()
        )));
    }

    let expectation = MarkerExpectation {
        volume_uuid: &unit.volume_uuid,
        resolved_path: &unit_path,
        effective_uid: observer.effective_uid().map_err(|error| {
            Refusal::refuse_unknown(format!("the account uid is unreadable: {error}"))
        })?,
        now_ms: state::now_ms(),
    };
    // A directory is opened too, so the identity the descriptor reports is the
    // object custody decided about, never whatever a racer put at the name; a
    // file has no descriptor-relative contents to walk, and its own facts above
    // are its binding.
    let unit_handle = if unit.kind == UnitKind::Directory {
        let handle = dirs.open_child_dir(&parent, &unit.name).map_err(|error| {
            Refusal::refuse_unknown(format!("the unit cannot be opened: {error}"))
        })?;
        let opened = handle.identity();
        if opened.device != unit.device || opened.inode != unit.inode {
            return Err(Refusal::refuse_unknown(format!(
                "the unit at {} opened as device {} inode {}, not the observed {} / {}",
                unit_path.display(),
                opened.device,
                opened.inode,
                unit.device,
                unit.inode
            )));
        }
        Some(handle)
    } else {
        None
    };
    let marker = verify_marker(dirs, unit, unit_handle.as_ref(), &expectation, &unit_path)?;

    // 4. The same-uid descriptor observation, complete within its bound.
    //
    //    The calling process holds descriptors inside the unit for the walk and
    //    for the checks that follow, so its own pid is excluded from the
    //    evidence: read once here, with the start identity the kernel reports
    //    for it, and verified by the platform as it scans. Nothing else is
    //    excluded, and an identity that cannot be read or does not match is a
    //    refusal, never a skip.
    let self_pid = std::process::id();
    let self_start = observer
        .process_start(self_pid)
        .map_err(|error| {
            Refusal::refuse_unknown(format!(
                "the calling process's start identity is unreadable: {error}"
            ))
        })?
        .ok_or_else(|| {
            Refusal::refuse_unknown(format!(
                "the calling process pid {self_pid} has no start identity, so its own descriptors cannot be excluded"
            ))
        })?;
    let scan = observer.scan_open_descriptors(&DescriptorScanRequest {
        root: &unit_path,
        root_device: unit.device,
        root_inode: unit.inode,
        max_processes: bounds.processes,
        max_descriptors: bounds.descriptors,
        timeout: bounds.timeout,
        self_exclusion: Some(SelfExclusion {
            pid: self_pid,
            start: self_start,
        }),
    });
    let (processes, descriptors, foreign, retry, os_blind_spot) = match scan {
        Err(error) => {
            return Err(Refusal::refuse_unknown(format!(
                "the open-descriptor scan is unreadable: {error}"
            )))
        }
        // A bounded scan proves nothing, and the bounded record of the `EPERM`
        // re-observation it did perform travels with the refusal.
        Ok(DescriptorScan::Bounded {
            processes,
            descriptors,
            bound,
            retry,
        }) => {
            return Err(Refusal::refuse_unknown(format!(
                "the open-descriptor scan reached its {bound} bound after {processes} processes and {descriptors} descriptors"
            ))
            .with_retry(retry))
        }
        // The observation of a same-uid process could not be completed: unknown,
        // never a skip, with the bounded retry record that led to it.
        Ok(DescriptorScan::Unreadable { reason, retry }) => {
            return Err(Refusal::refuse_unknown(format!(
                "the open-descriptor scan is unreadable: {reason}"
            ))
            .with_retry(retry))
        }
        Ok(DescriptorScan::Complete {
            processes,
            descriptors,
            foreign,
            retry,
            os_blind_spot,
            open,
        }) => {
            if let Some(first) = open.first() {
                return Err(Refusal::refuse_unsafe(format!(
                    "pid {} holds descriptor {} open at {}",
                    first.pid,
                    first.descriptor,
                    first.path.display()
                ))
                .with_retry(retry));
            }
            (processes, descriptors, foreign, retry, os_blind_spot)
        }
    };

    Ok(CustodyEvidence {
        unit_name: unit.name.clone(),
        unit_kind: unit.kind.as_str().to_owned(),
        parent: unit.parent.clone(),
        volume_uuid: unit.volume_uuid.clone(),
        device: unit.device.get(),
        inode: unit.inode,
        marker,
        grant: unit.grant.clone(),
        holds: holds.total,
        holds_stale: stale,
        processes,
        descriptors,
        self_excluded: self_pid,
        foreign_uid_processes: foreign,
        retry,
        os_blind_spot,
        checked_ms: state::now_ms(),
    })
}

/// Whether the grant the unit records is still recorded on the volume.
///
/// A native root needs no entry: Barge's own marker is its grant, checked as a
/// marker. A foreign family needs that family's delegation in the volume's
/// `scratch_grants`, and an unmarked unit needs a claim whose scope covers it -
/// `project:<name>` for a child of that project folder, `top_level_adhoc` for an
/// entry directly under the scratch root - so a grant revoked between the plan and
/// the effect keeps the unit rather than being trusted from the plan.
fn grant_still_recorded(
    record: &registry::VolumeRecord,
    unit: &DeletionUnit,
) -> std::result::Result<(), String> {
    match unit.grant.kind {
        GrantKind::NativeRoot => Ok(()),
        GrantKind::Delegation => {
            let Some(family) = MarkerFamily::parse(&unit.grant.reference) else {
                return Err(format!(
                    "`{}` is not a marker family a delegation can cover",
                    unit.grant.reference
                ));
            };
            if record
                .scratch_grants
                .delegations
                .iter()
                .any(|delegation| MarkerFamily::from(delegation.family) == family)
            {
                return Ok(());
            }
            Err(format!(
                "volume {} records no `{}` delegation, so the unit `{}` is kept",
                record.volume_uuid,
                family.as_str(),
                unit.name
            ))
        }
        GrantKind::Claim => {
            let covers = record
                .scratch_grants
                .claims
                .iter()
                .any(|claim| match &claim.scope {
                    registry::ClaimScope::TopLevelAdhoc => {
                        unit.grant.reference == "top_level_adhoc" && unit.parent.is_empty()
                    }
                    registry::ClaimScope::Project { name } => {
                        unit.grant.reference == format!("project:{name}") && unit.parent == *name
                    }
                });
            if covers {
                return Ok(());
            }
            Err(format!(
                "volume {} records no `{}` claim covering the unit `{}` in `{}`, so it is kept",
                record.volume_uuid, unit.grant.reference, unit.name, unit.parent
            ))
        }
    }
}

/// Verify the marker a unit records, or that a markerless unit stayed unmarked.
///
/// A Barge marker is read by [`area::read_marker_with_digest`], so it must be a
/// `barge.root/1` document naming this unit; a foreign family's marker is read
/// as bytes at that family's own filename, and its digest is compared with the
/// plan's. A directory unit that records no marker carries none of the three
/// recognised families: a unit that became marked between the plan and the effect
/// is kept, never deleted under a claim that no longer describes it.
fn verify_marker(
    dirs: &dyn DirOps,
    unit: &DeletionUnit,
    handle: Option<&crate::os::DirHandle>,
    expectation: &MarkerExpectation<'_>,
    unit_path: &Path,
) -> std::result::Result<Option<UnitMarker>, Refusal> {
    let Some(observed) = &unit.marker else {
        // A file unit never carries a marker (validated on the unit), so there
        // is nothing inside it to check.
        if unit.kind == UnitKind::File {
            return Ok(None);
        }
        let Some(handle) = handle else {
            return Err(Refusal::refuse_unknown(
                "the unit records no marker but has no directory to check".to_owned(),
            ));
        };
        for filename in unit::RECOGNISED_MARKER_FILES {
            match dirs.child_facts(handle, filename) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit at {} now carries `{filename}`, so it is not the unmarked unit the plan named",
                        unit_path.display()
                    )))
                }
                Err(error) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit's `{filename}` cannot be read: {error}"
                    )))
                }
            }
        }
        return Ok(None);
    };
    let Some(handle) = handle else {
        return Err(Refusal::refuse_unknown(format!(
            "the unit records the `{}` marker but is not a directory that holds it",
            observed.family.as_str()
        )));
    };
    match observed.family {
        MarkerFamily::Barge => {
            let (marker, digest) = area::read_marker_with_digest(dirs, handle, expectation);
            match marker {
                MarkerState::Owned(owned) if owned.root_id == unit.name => {}
                MarkerState::Owned(other) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the marker names root {}, not the observed `{}`",
                        other.root_id, unit.name
                    )))
                }
                MarkerState::Absent => {
                    return Err(Refusal::refuse_unknown(
                        "the unit carries no marker, so it is not owned".to_owned(),
                    ))
                }
                MarkerState::Unknown { reason } => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit's marker is not owned: {reason}"
                    )))
                }
            }
            let digest = digest.ok_or_else(|| {
                Refusal::refuse_unknown("the unit's marker digest could not be read".to_owned())
            })?;
            if digest != observed.digest {
                return Err(Refusal::refuse_unknown(format!(
                    "the marker at {} is digest {digest}, not the observed {}",
                    unit_path.display(),
                    observed.digest
                )));
            }
        }
        family => {
            let facts = match dirs.child_facts(handle, &observed.filename) {
                Ok(Some(facts)) => facts,
                Ok(None) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit's `{}` marker is not there",
                        observed.filename
                    )))
                }
                Err(error) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit's `{}` marker cannot be read: {error}",
                        observed.filename
                    )))
                }
            };
            if facts.kind != EntryKind::File {
                return Err(Refusal::refuse_unknown(format!(
                    "the unit's `{}` marker is a {}, not a regular file",
                    observed.filename,
                    facts.kind.as_str()
                )));
            }
            if facts.uid != expectation.effective_uid {
                return Err(Refusal::refuse_unknown(format!(
                    "the unit's `{}` marker belongs to uid {}, not this account's {}",
                    observed.filename, facts.uid, expectation.effective_uid
                )));
            }
            if facts.size > MARKER_CAP_BYTES as u64 {
                return Err(Refusal::refuse_unknown(format!(
                    "the unit's `{}` marker is {} bytes, over the {MARKER_CAP_BYTES}-byte cap",
                    observed.filename, facts.size
                )));
            }
            let bytes = match dirs.read_child_file(handle, &observed.filename, MARKER_CAP_BYTES) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit's `{}` marker is not there",
                        observed.filename
                    )))
                }
                Err(error) => {
                    return Err(Refusal::refuse_unknown(format!(
                        "the unit's `{}` marker cannot be read: {error}",
                        observed.filename
                    )))
                }
            };
            let digest = crate::digest::sha256_hex(&bytes);
            if digest != observed.digest {
                return Err(Refusal::refuse_unknown(format!(
                    "the unit's `{}` marker is digest {digest}, not the observed {}",
                    observed.filename, observed.digest
                )));
            }
            let _ = family;
        }
    }
    Ok(Some(observed.clone()))
}

/// What `describe.custody` reports: the capability, what it establishes, what it
/// does not, and that no kernel proof is claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyReport {
    /// [`CAPABILITY`], so a reader sees the exact capability string.
    pub capability: String,
    /// The facts the verdict is built from.
    pub establishes: Vec<String>,
    /// The cases it does not observe, named rather than implied.
    pub excludes: Vec<String>,
    /// Always false: Barge claims no kernel proof that no writer exists.
    pub kernel_proof: bool,
    /// The only tools that are diagnostics and never verdicts.
    pub diagnostics: Vec<String>,
}

impl Default for CustodyReport {
    fn default() -> Self {
        capability_report()
    }
}

/// The custody capability `describe` reports, honestly.
pub fn capability_report() -> CustodyReport {
    CustodyReport {
        capability: CAPABILITY.to_owned(),
        establishes: vec![
            "every recorded hold for the deletion unit is released or positively stale; Barge holds are keyed by a Barge root id, so a unit Barge never created has none to read".to_owned(),
            "a bounded scan of this account's open descriptors is complete and finds none at or under the unit".to_owned(),
            "that scan classifies each process by the effective uid the kernel's own process table reports, before it inspects any descriptor, and reports how many processes of another effective uid it classified".to_owned(),
            "that scan excludes the calling barge process's own descriptors, by its pid and the start identity read once and verified, and no other process: another barge process, its child or its parent is observed like any other same-uid writer".to_owned(),
            "that scan re-observes a same-uid process whose descriptor listing the kernel answers EPERM for, by re-reading the kernel's pid, start identity and effective uid and listing again at most three times inside a 250 ms settle window, with both that window and the scan's own deadline rechecked after every backoff, and settles it only on a positive fact: a later listing, a process the kernel reports gone, a changed start identity re-evaluated as the different process it now names, or a changed effective uid counted as the named blind spot; the same identity still answering EPERM is unknown and refuses, and elapsed time is never taken as proof".to_owned(),
            "that scan treats a vnode descriptor whose path and vnode facts the platform both answer EPERM for as an OS blind spot, counted and reported in the evidence and never a refusal, only when the process's own code-signing status carries the platform binary flag and its kernel start identity is unchanged before and after that read; a process that is not a verified platform binary, a status read that fails, and an identity that changes around it leave those descriptors unknown".to_owned(),
            "that evidence names every blind-spot process it counts — pid, kernel start identity, command and hidden-descriptor count — up to 32 of them, and a scan that meets more refuses rather than report a count it cannot show".to_owned(),
            "the bounded record of that re-observation — how many processes it re-observed, and for up to eight of them the command, kernel start identity, errno sequence, identity changes and resolution — is reported on every verdict and in every refusal's failure detail, so a diagnostic of a refusal sees it too".to_owned(),
            "the unit's device, inode and kind equal the observation the caller was given".to_owned(),
            "the unit's marker, when it records one, still holds exactly the bytes the caller observed at its family's own filename; a unit that records no marker carries none of the three recognised families, so a unit that became marked is kept".to_owned(),
            "the recorded grant is the form the unit's marker family needs - a native root for Barge's own marker, that family's delegation for a foreign marker, and a legacy claim for an unmarked unit - and is still recorded on the volume when custody decides, so no delete path skips a grant or trusts one revoked since the plan".to_owned(),
            "the volume still verifies as the registered one".to_owned(),
            "no pending intent names another unit".to_owned(),
        ],
        excludes: vec![
            "a process of another effective uid: classified from the kernel's process table and reported in the evidence as not observed, never inspected".to_owned(),
            "a vnode descriptor whose path the platform hides on a process it verifies as a platform binary: counted and reported as the OS blind spot, and not observed, because nothing Barge can read says where it points".to_owned(),
            "a process created after the scan".to_owned(),
            "a writer that never took a hold and stopped being observable".to_owned(),
        ],
        kernel_proof: false,
        diagnostics: vec!["lsof".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        capability_report, take_custody, CustodyBounds, CustodyVerdict, PendingIntent, CAPABILITY,
    };
    use crate::area::RootKind;
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::{
        platform, retry_schedule, DescriptorScan, DescriptorScanRequest, OpenDescriptor,
        PlatformStatus, ProcessIdentity, ProcessObserver, RetryResolution, StatedListing,
        StatedPlatform, VolumeUuid,
    };
    use crate::testkit::{Faults, ScanMode, VolumeFixture, FIXTURE_BOOT, OTHER_UUID};
    use crate::unit::DeletionUnit;

    /// The plan digest every custody fixture was authorised by.
    const PLAN: &str = "abababababababababababababababababababababababababababababababab";

    /// `EPERM`, the intermittent errno the kernel answers for a descriptor
    /// listing it will not show, and `EACCES`, an answer the retry rule does not
    /// settle. The scan reports both exactly as the kernel does.
    const EPERM: i32 = 1;
    const ESRCH: i32 = 3;
    const EACCES: i32 = 13;

    struct Holder {
        fixture: VolumeFixture,
        unit: DeletionUnit,
        /// The area folder the unit lives in.
        area: PathBuf,
    }

    fn holder(label: &str) -> Holder {
        build(VolumeFixture::new(label))
    }

    /// A holder whose fixture lives under the scratch base (AGENTS.md): the
    /// real-observer tests run there.
    fn real_holder(label: &str) -> Holder {
        build(VolumeFixture::under_scratch(label))
    }

    fn build(fixture: VolumeFixture) -> Holder {
        fixture.register();
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) =
            fixture.create_root(&fixture.record, &declaration, RootKind::AreaRoot);
        let unit = DeletionUnit::from_owned(&root, "", "example/pool", PLAN);
        let area = resolved.path.clone();
        Holder {
            fixture,
            unit,
            area,
        }
    }

    /// A same-uid child process that holds a file under the root open, so the
    /// scan has another writer to find. It is killed and reaped by [`Child`].
    struct Child {
        process: std::process::Child,
    }

    impl Child {
        /// Spawn `/bin/sleep` with the file at `path` as its stdin, so the child
        /// holds one descriptor at that vnode for as long as it runs.
        fn holding(path: &std::path::Path) -> Self {
            let held = std::fs::File::open(path).expect("the held payload opens");
            let process = std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(std::process::Stdio::from(held))
                .spawn()
                .expect("a same-uid child holds the payload open");
            Self { process }
        }

        /// Spawn `/bin/sleep` holding nothing under the root: a live same-uid
        /// process whose listing can be stated unreadable, and whose own handles
        /// are not a writer's claim on the root.
        fn idle() -> Self {
            let process = std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .spawn()
                .expect("a same-uid child runs");
            Self { process }
        }

        /// Spawn `/bin/sleep` holding two files under the root open, on stdin and
        /// stdout, so one of its descriptors can be stated opaque while another
        /// stays readable.
        fn holding_two(first: &std::path::Path, second: &std::path::Path) -> Self {
            let first = std::fs::File::open(first).expect("the first held file opens");
            let second = std::fs::File::open(second).expect("the second held file opens");
            let process = std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(std::process::Stdio::from(first))
                .stdout(std::process::Stdio::from(second))
                .spawn()
                .expect("a same-uid child holds both files open");
            Self { process }
        }

        /// The child's own identity, as the kernel's table reports it.
        fn identity(&self) -> ProcessIdentity {
            let pid = self.process.id();
            let start = platform()
                .process_start(pid)
                .expect("the child's start identity is readable")
                .expect("the child exists");
            ProcessIdentity {
                pid,
                effective_uid: Some(platform().effective_uid().expect("the account uid")),
                start,
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    /// The calling process's own identity, as the kernel's table reports it.
    fn self_identity() -> ProcessIdentity {
        let pid = std::process::id();
        ProcessIdentity {
            pid,
            effective_uid: Some(platform().effective_uid().expect("the account uid")),
            start: platform()
                .process_start(pid)
                .expect("the calling process's start identity")
                .expect("the calling process exists"),
        }
    }

    /// The real macOS observer, given one process table a test states itself.
    ///
    /// The classification, the descriptor enumeration, the path matching, the
    /// self-exclusion and the bounded re-observation of an `EPERM` are the
    /// production ones; only the table and the stated `EPERM` answers are
    /// injected, which is how an unprivileged fact about another account, an
    /// unreadable uid, a table that claims a pid this account cannot read, and an
    /// intermittent descriptor listing are exercised against the real kernel
    /// reads.
    struct TableObserver {
        inner: crate::os::MacOs,
        table: Vec<ProcessIdentity>,
        /// The `EPERM` answers and identity overrides the test states, consumed
        /// by the scan.
        faults: Vec<StatedListing>,
        /// The backoff schedule the scan retries on: the production one, or the
        /// empty schedule of the counterfactual.
        backoffs: Vec<Duration>,
    }

    impl TableObserver {
        fn new(table: Vec<ProcessIdentity>) -> Self {
            Self {
                inner: platform(),
                table,
                faults: Vec::new(),
                backoffs: retry_schedule().to_vec(),
            }
        }

        /// The same observer with a stated `EPERM` sequence.
        fn stated(table: Vec<ProcessIdentity>, faults: Vec<StatedListing>) -> Self {
            Self {
                faults,
                ..Self::new(table)
            }
        }

        /// The same observer with a stated `EPERM` sequence and an empty backoff
        /// schedule: the counterfactual, where nothing is retried.
        fn without_retries(table: Vec<ProcessIdentity>, faults: Vec<StatedListing>) -> Self {
            Self {
                faults,
                backoffs: Vec::new(),
                ..Self::new(table)
            }
        }
    }

    impl ProcessObserver for TableObserver {
        fn effective_uid(&self) -> Result<u32> {
            self.inner.effective_uid()
        }

        fn boot_identity(&self) -> Result<String> {
            self.inner.boot_identity()
        }

        fn process_start(&self, pid: u32) -> Result<Option<u64>> {
            self.inner.process_start(pid)
        }

        fn verify_platform_binary(&self, classified: ProcessIdentity) -> Result<PlatformStatus> {
            // The stated answers reach the scan itself, which is where the
            // exemption is decided; this is the production read.
            self.inner.verify_platform_binary(classified)
        }

        fn scan_open_descriptors(
            &self,
            request: &DescriptorScanRequest<'_>,
        ) -> Result<DescriptorScan> {
            // The stated answers are consumed, so each scan reads its own copy.
            let mut faults = self.faults.clone();
            self.inner
                .scan_open_descriptors_with(request, &self.table, &self.backoffs, &mut faults)
        }
    }

    /// The real observer with the self-exclusion removed: the counterfactual
    /// that shows the exclusion is what makes the calling process's own handles
    /// invisible.
    struct NoSelfExclusion {
        inner: crate::os::MacOs,
        table: Vec<ProcessIdentity>,
    }

    impl ProcessObserver for NoSelfExclusion {
        fn effective_uid(&self) -> Result<u32> {
            self.inner.effective_uid()
        }

        fn boot_identity(&self) -> Result<String> {
            self.inner.boot_identity()
        }

        fn process_start(&self, pid: u32) -> Result<Option<u64>> {
            self.inner.process_start(pid)
        }

        fn verify_platform_binary(&self, classified: ProcessIdentity) -> Result<PlatformStatus> {
            self.inner.verify_platform_binary(classified)
        }

        fn scan_open_descriptors(
            &self,
            request: &DescriptorScanRequest<'_>,
        ) -> Result<DescriptorScan> {
            self.inner.scan_open_descriptors_over(
                &DescriptorScanRequest {
                    self_exclusion: None,
                    ..*request
                },
                &self.table,
            )
        }
    }

    /// The real observer whose recorded start identity for the calling process
    /// is wrong, which the scan must refuse rather than skip.
    struct MismatchedSelf {
        inner: crate::os::MacOs,
        table: Vec<ProcessIdentity>,
    }

    impl ProcessObserver for MismatchedSelf {
        fn effective_uid(&self) -> Result<u32> {
            self.inner.effective_uid()
        }

        fn boot_identity(&self) -> Result<String> {
            self.inner.boot_identity()
        }

        fn process_start(&self, pid: u32) -> Result<Option<u64>> {
            let start = self.inner.process_start(pid)?;
            if pid == std::process::id() {
                return start
                    .map(|start| Some(start.saturating_add(1)))
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::UnknownState,
                            "the calling process has no start identity",
                        )
                    });
            }
            Ok(start)
        }

        fn verify_platform_binary(&self, classified: ProcessIdentity) -> Result<PlatformStatus> {
            self.inner.verify_platform_binary(classified)
        }

        fn scan_open_descriptors(
            &self,
            request: &DescriptorScanRequest<'_>,
        ) -> Result<DescriptorScan> {
            self.inner.scan_open_descriptors_over(request, &self.table)
        }
    }

    impl Holder {
        fn hold(&self, holder: &str, pid: u32, process_start: u64, boot: &str) {
            let path = self
                .fixture
                .state
                .holds_dir()
                .join(&self.unit.name)
                .join(format!("{holder}.json"));
            std::fs::create_dir_all(path.parent().expect("the hold parent"))
                .expect("the holds dir");
            std::fs::write(
                &path,
                VolumeFixture::hold_record(&self.unit.name, holder, pid, process_start, boot),
            )
            .expect("the hold is written");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("the hold is private");
        }

        fn take(
            &self,
            pending: &[PendingIntent],
            observer: &crate::testkit::FakeObserver,
        ) -> CustodyVerdict {
            self.take_with(pending, observer, CustodyBounds::default())
        }

        fn take_with(
            &self,
            pending: &[PendingIntent],
            observer: &dyn ProcessObserver,
            bounds: CustodyBounds,
        ) -> CustodyVerdict {
            let faults = Faults::new();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            take_custody(
                &faults,
                &facts,
                &mounts,
                observer,
                &self.fixture.state,
                &self.unit,
                pending,
                bounds,
            )
        }

        /// The unit's absolute path, as the walk reached it.
        fn root_path(&self) -> PathBuf {
            self.unit.path_in(&self.area)
        }

        /// A payload file inside the root, whose open descriptor is a writer's
        /// claim on the root.
        fn payload(&self) -> PathBuf {
            let path = self.root_path().join("payload.bin");
            std::fs::write(&path, b"payload").expect("the payload is written");
            path
        }

        fn safe(&self) -> CustodyVerdict {
            self.take(&[], &self.fixture.observer())
        }

        /// Decide custody of any unit in this holder's area folder.
        fn take_unit(&self, unit: &DeletionUnit) -> CustodyVerdict {
            let faults = Faults::new();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            take_custody(
                &faults,
                &facts,
                &mounts,
                &self.fixture.observer(),
                &self.fixture.state,
                unit,
                &[],
                CustodyBounds::default(),
            )
        }

        /// Publish the volume's recorded grants, as S4's operator declarations do.
        fn record_grants(&self, grants: crate::registry::ScratchGrants) {
            let mut record = self.fixture.record.clone();
            record.scratch_grants = grants;
            self.fixture.register_record(&record);
        }

        /// A planted unit in the holder's area folder, with the plan's identity
        /// read from the filesystem and the grant the plan recorded.
        fn planted_unit(
            &self,
            parent: &str,
            name: &str,
            marker: Option<crate::unit::UnitMarker>,
            grant: crate::unit::UnitGrant,
        ) -> DeletionUnit {
            let path = if parent.is_empty() {
                self.area.join(name)
            } else {
                self.area.join(parent).join(name)
            };
            let Some(facts) =
                crate::os::DirOps::path_facts(&platform(), &path).expect("the planted unit reads")
            else {
                panic!("the planted unit exists");
            };
            let kind = crate::unit::UnitKind::from_entry_kind(facts.kind)
                .expect("a planted unit is a file or a directory");
            DeletionUnit {
                volume_uuid: self.fixture.uuid.clone(),
                root_dir: String::new(),
                folder: "example/pool".to_owned(),
                parent: parent.to_owned(),
                name: name.to_owned(),
                kind,
                device: facts.device,
                inode: facts.inode,
                marker,
                grant,
                plan_digest: PLAN.to_owned(),
            }
        }
    }

    /// The delegation a foreign family's deletion rests on.
    fn delegation(family: crate::unit::MarkerFamily) -> crate::registry::ScratchGrants {
        let family = match family {
            crate::unit::MarkerFamily::EffigyScratch => {
                crate::registry::MarkerFamily::EffigyScratch
            }
            crate::unit::MarkerFamily::OxideScratch => crate::registry::MarkerFamily::OxideScratch,
            crate::unit::MarkerFamily::Barge => panic!("a Barge root needs no delegation"),
        };
        crate::registry::ScratchGrants {
            delegations: vec![crate::registry::ScratchDelegation {
                family,
                by: "the family's owner".to_owned(),
                declared_ms: 1,
            }],
            claims: Vec::new(),
        }
    }

    /// The claim an unmarked unit's deletion rests on.
    fn claim(scope: crate::registry::ClaimScope) -> crate::registry::ScratchGrants {
        crate::registry::ScratchGrants {
            delegations: Vec::new(),
            claims: vec![crate::registry::ScratchClaim {
                scope,
                unit: crate::registry::ClaimUnit::Child,
                unmarked_idle_seconds: 1_209_600,
                families_excluded: vec![
                    crate::registry::MarkerFamily::EffigyScratch,
                    crate::registry::MarkerFamily::OxideScratch,
                ],
                declared_by: "the operator".to_owned(),
                declared_ms: 1,
            }],
        }
    }

    #[test]
    fn a_claimed_unmarked_unit_with_a_complete_empty_scan_is_safe() {
        let holder = holder("custody-unmarked");
        holder.record_grants(claim(crate::registry::ClaimScope::Project {
            name: "alpha".to_owned(),
        }));
        std::fs::create_dir(holder.area.join("alpha")).expect("the project folder exists");
        std::fs::create_dir(holder.area.join("alpha").join("unmarked-run"))
            .expect("the unit is planted");
        let unit = holder.planted_unit(
            "alpha",
            "unmarked-run",
            None,
            crate::unit::UnitGrant::claim("project:alpha"),
        );
        let verdict = holder.take_unit(&unit);
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.into_evidence().expect("the evidence");
        assert_eq!(evidence.unit_name, "unmarked-run");
        assert_eq!(evidence.unit_kind, "directory");
        assert_eq!(evidence.marker, None);
        assert_eq!(
            evidence.grant,
            crate::unit::UnitGrant::claim("project:alpha")
        );
    }

    #[test]
    fn a_claimed_file_unit_with_a_complete_empty_scan_is_safe() {
        let holder = holder("custody-file");
        holder.record_grants(claim(crate::registry::ClaimScope::TopLevelAdhoc));
        std::fs::write(holder.area.join("report.txt"), b"bytes").expect("the unit is planted");
        let unit = holder.planted_unit(
            "",
            "report.txt",
            None,
            crate::unit::UnitGrant::claim("top_level_adhoc"),
        );
        assert_eq!(unit.kind, crate::unit::UnitKind::File);
        let verdict = holder.take_unit(&unit);
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.into_evidence().expect("the evidence");
        assert_eq!(evidence.unit_kind, "file");
        assert_eq!(evidence.inode, unit.inode);
    }

    #[test]
    fn a_claimed_unmarked_unit_that_becomes_marked_is_refused() {
        let holder = holder("custody-became-marked");
        holder.record_grants(claim(crate::registry::ClaimScope::Project {
            name: "alpha".to_owned(),
        }));
        std::fs::create_dir(holder.area.join("alpha")).expect("the project folder exists");
        std::fs::create_dir(holder.area.join("alpha").join("unmarked-run"))
            .expect("the unit is planted");
        let unit = holder.planted_unit(
            "alpha",
            "unmarked-run",
            None,
            crate::unit::UnitGrant::claim("project:alpha"),
        );
        std::fs::write(
            holder
                .area
                .join("alpha")
                .join("unmarked-run")
                .join(crate::unit::EFFIGY_SCRATCH_MARKER_FILE),
            b"{\"run\": \"late\"}",
        )
        .expect("a marker appears");
        let verdict = holder.take_unit(&unit);
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(
            verdict.reason().unwrap_or_default().contains("now carries"),
            "{:?}",
            verdict.reason()
        );
    }

    #[test]
    fn a_delegated_effigy_unit_is_safe_and_its_moved_marker_bytes_are_not() {
        let holder = holder("custody-effigy");
        holder.record_grants(delegation(crate::unit::MarkerFamily::EffigyScratch));
        let unit_path = holder.area.join("run.effigy");
        std::fs::create_dir(&unit_path).expect("the unit is planted");
        let marker_file = crate::unit::MarkerFamily::EffigyScratch.marker_file();
        std::fs::write(unit_path.join(marker_file), b"{\"run\": \"effigy\"}")
            .expect("the marker is written");
        let digest = crate::digest::sha256_hex(b"{\"run\": \"effigy\"}");
        let marker = crate::unit::UnitMarker::new(crate::unit::MarkerFamily::EffigyScratch, digest);
        let unit = holder.planted_unit(
            "",
            "run.effigy",
            Some(marker.clone()),
            crate::unit::UnitGrant::delegation(crate::unit::MarkerFamily::EffigyScratch),
        );
        let verdict = holder.take_unit(&unit);
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        assert_eq!(
            verdict
                .evidence()
                .and_then(|evidence| evidence.marker.clone()),
            Some(marker.clone())
        );

        std::fs::write(unit_path.join(marker_file), b"{\"run\": \"moved\"}")
            .expect("the marker moves");
        let verdict = holder.take_unit(&unit);
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict.reason().unwrap_or_default().contains("digest"));
    }

    #[test]
    fn a_foreign_unit_whose_grant_is_not_recorded_on_the_volume_is_kept() {
        // A delegation that the volume does not record covers nothing, so custody
        // refuses even though the unit's own record names it: a grant revoked
        // between the plan and the effect keeps the unit.
        let holder = holder("custody-grant-revoked");
        let unit_path = holder.area.join("run.effigy");
        std::fs::create_dir(&unit_path).expect("the unit is planted");
        let marker =
            crate::unit::UnitMarker::new(crate::unit::MarkerFamily::EffigyScratch, "a".repeat(64));
        let unit = holder.planted_unit(
            "",
            "run.effigy",
            Some(marker),
            crate::unit::UnitGrant::delegation(crate::unit::MarkerFamily::EffigyScratch),
        );
        let verdict = holder.take_unit(&unit);
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(
            verdict
                .reason()
                .unwrap_or_default()
                .contains("records no `effigy_scratch` delegation"),
            "{:?}",
            verdict.reason()
        );
    }

    #[test]
    fn a_unit_whose_grant_does_not_cover_its_marker_family_is_refused() {
        let holder = holder("custody-grant-shape");
        holder.record_grants(delegation(crate::unit::MarkerFamily::EffigyScratch));
        let unit_path = holder.area.join("run.effigy");
        std::fs::create_dir(&unit_path).expect("the unit is planted");
        let marker =
            crate::unit::UnitMarker::new(crate::unit::MarkerFamily::EffigyScratch, "a".repeat(64));
        let mut unit = holder.planted_unit(
            "",
            "run.effigy",
            Some(marker),
            crate::unit::UnitGrant::delegation(crate::unit::MarkerFamily::EffigyScratch),
        );
        // A foreign family is never covered by a claim, and a native root never by
        // a delegation: the custody verdict refuses before it scans.
        unit.grant = crate::unit::UnitGrant::claim("top_level_adhoc");
        assert_eq!(holder.take_unit(&unit).name(), "unknown");
    }

    #[test]
    fn a_released_root_with_a_complete_empty_scan_is_safe() {
        let holder = holder("custody-safe");
        let verdict = holder.safe();
        assert_eq!(verdict.name(), "safe");
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.unit_name, holder.unit.name);
        assert_eq!(evidence.unit_kind, "directory");
        assert_eq!(evidence.holds, 0);
        assert_eq!(evidence.marker, holder.unit.marker);
        // The authority is a type: a refusal cannot become evidence.
        assert!(verdict.into_evidence().is_ok());
    }

    #[test]
    fn a_live_hold_is_unsafe() {
        let holder = holder("custody-live");
        let pid = std::process::id();
        holder.hold("writer", pid, 7_000_000, FIXTURE_BOOT);
        let verdict = holder.safe();
        assert_eq!(verdict.name(), "unsafe", "{:?}", verdict.reason());
        assert!(!verdict.is_safe());
        assert!(verdict.into_evidence().is_err());
    }

    #[test]
    fn a_hold_with_a_mismatched_start_identity_is_unknown_not_stale() {
        let holder = holder("custody-reused-pid");
        holder.hold("writer", std::process::id(), 1, FIXTURE_BOOT);
        let verdict = holder.safe();
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict.into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn an_unreadable_hold_is_unknown() {
        let holder = holder("custody-unreadable-hold");
        let path = holder
            .fixture
            .state
            .holds_dir()
            .join(&holder.unit.name)
            .join("writer.json");
        std::fs::create_dir_all(path.parent().expect("the hold parent")).expect("the holds dir");
        std::fs::write(&path, b"{ not a hold").expect("the hold is written");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");
        let verdict = holder.safe();
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
    }

    #[test]
    fn foreign_uid_evidence_is_unknown() {
        let holder = holder("custody-foreign-uid");
        holder.hold("writer", std::process::id(), 7_000_000, FIXTURE_BOOT);
        let mut observer = holder.fixture.observer();
        observer.uid = observer.uid.saturating_add(1);
        let verdict = holder.take(&[], &observer);
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
    }

    #[test]
    fn a_stale_hold_does_not_block() {
        let holder = holder("custody-stale");
        holder.hold("old-boot", std::process::id(), 7_000_000, "1600000000.0");
        let verdict = holder.safe();
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        assert_eq!(verdict.evidence().expect("the evidence").holds_stale, 1);
    }

    #[test]
    fn a_bounded_scan_is_unknown() {
        let holder = holder("custody-bounded");
        let mut observer = holder.fixture.observer();
        observer.scan = ScanMode::Bounded;
        let verdict = holder.take(&[], &observer);
        assert_eq!(verdict.name(), "unknown");
        assert!(verdict.reason().unwrap_or_default().contains("bound"));
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn an_unreadable_scan_is_unknown() {
        let holder = holder("custody-scan-unreadable");
        let mut observer = holder.fixture.observer();
        observer.scan = ScanMode::Unreadable;
        let verdict = holder.take(&[], &observer);
        assert_eq!(verdict.name(), "unknown");
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn an_open_descriptor_is_unsafe() {
        let holder = holder("custody-open");
        let mut observer = holder.fixture.observer();
        observer.scan = ScanMode::Open(vec![OpenDescriptor {
            pid: 4_242,
            descriptor: 7,
            path: PathBuf::from("/fixture/payload.txt"),
        }]);
        let verdict = holder.take(&[], &observer);
        assert_eq!(verdict.name(), "unsafe");
        assert!(verdict.reason().unwrap_or_default().contains("4242"));
    }

    #[test]
    fn a_pending_intent_naming_another_root_is_unknown() {
        let holder = holder("custody-other-intent");
        let pending = [PendingIntent {
            intent_id: "a".repeat(32),
            parent: String::new(),
            name: "b".repeat(32),
        }];
        let verdict = holder.take(&pending, &holder.fixture.observer());
        assert_eq!(verdict.name(), "unknown");
        assert!(verdict.reason().unwrap_or_default().contains("must wait"));
    }

    #[test]
    fn a_pending_intent_naming_this_root_is_unknown() {
        let holder = holder("custody-own-intent");
        let pending = [PendingIntent {
            intent_id: "a".repeat(32),
            parent: holder.unit.parent.clone(),
            name: holder.unit.name.clone(),
        }];
        let verdict = holder.take(&pending, &holder.fixture.observer());
        assert_eq!(verdict.name(), "unknown");
        assert!(verdict.reason().unwrap_or_default().contains("already"));
    }

    #[test]
    fn a_marker_digest_that_moved_is_unknown() {
        let holder = holder("custody-digest");
        let mut observed = holder.unit.clone();
        observed.marker = Some(crate::unit::UnitMarker::new(
            crate::unit::MarkerFamily::Barge,
            "0".repeat(64),
        ));
        let faults = Faults::new();
        let facts = holder.fixture.facts();
        let mounts = holder.fixture.mounts();
        let verdict = take_custody(
            &faults,
            &facts,
            &mounts,
            &holder.fixture.observer(),
            &holder.fixture.state,
            &observed,
            &[],
            CustodyBounds::default(),
        );
        assert_eq!(verdict.name(), "unknown");
    }

    #[test]
    fn a_root_id_that_moved_is_unknown() {
        let holder = holder("custody-root-moved");
        let mut observed = holder.unit.clone();
        observed.inode = observed.inode.saturating_add(1);
        let faults = Faults::new();
        let facts = holder.fixture.facts();
        let mounts = holder.fixture.mounts();
        let verdict = take_custody(
            &faults,
            &facts,
            &mounts,
            &holder.fixture.observer(),
            &holder.fixture.state,
            &observed,
            &[],
            CustodyBounds::default(),
        );
        assert_eq!(verdict.name(), "unknown");
    }

    #[test]
    fn a_different_volume_is_unknown() {
        let holder = holder("custody-volume");
        let mut observed = holder.unit.clone();
        observed.volume_uuid = VolumeUuid::parse(OTHER_UUID).expect("a UUID");
        // A native-root grant names its own unit; the recorded grant is untouched
        // here, because the volume is what the test moved.
        let faults = Faults::new();
        let facts = holder.fixture.facts();
        let mounts = holder.fixture.mounts();
        let verdict = take_custody(
            &faults,
            &facts,
            &mounts,
            &holder.fixture.observer(),
            &holder.fixture.state,
            &observed,
            &[],
            CustodyBounds::default(),
        );
        assert_eq!(verdict.name(), "unknown");
    }

    #[test]
    fn the_capability_report_names_its_limits_and_claims_no_kernel_proof() {
        let report = capability_report();
        assert_eq!(report.capability, CAPABILITY);
        assert!(!report.kernel_proof);
        assert!(report.diagnostics.iter().any(|tool| tool == "lsof"));
        assert!(report
            .establishes
            .iter()
            .any(|fact| fact.contains("open descriptors")));
        assert!(report
            .excludes
            .iter()
            .any(|fact| fact.contains("another effective uid")));
        assert!(
            report
                .establishes
                .iter()
                .any(|fact| fact.contains("calling barge process")),
            "the capability names the one exclusion the scan makes: {:?}",
            report.establishes
        );
        assert!(
            report
                .establishes
                .iter()
                .any(|fact| fact.contains("effective uid")),
            "the capability names the classification the scan makes: {:?}",
            report.establishes
        );
        assert!(
            report
                .establishes
                .iter()
                .any(|fact| fact.contains("EPERM") && fact.contains("positive fact")),
            "the capability names the bounded re-observation and that it settles only on a positive fact: {:?}",
            report.establishes
        );
        assert!(
            report
                .establishes
                .iter()
                .any(|fact| fact.contains("errno sequence")),
            "the capability names the bounded record each retried pid leaves: {:?}",
            report.establishes
        );
        assert!(
            report
                .establishes
                .iter()
                .any(|fact| fact.contains("platform binary flag") && fact.contains("OS blind spot")),
            "the capability names the verified platform binary exemption and its condition: {:?}",
            report.establishes
        );
        assert!(
            report
                .establishes
                .iter()
                .any(|fact| fact.contains("names every blind-spot process")),
            "the capability says the evidence names each blind-spot process rather than a count it cannot show: {:?}",
            report.establishes
        );
        assert!(
            report
                .excludes
                .iter()
                .any(|fact| fact.contains("platform binary") && fact.contains("OS blind spot")),
            "the capability names the blind spot as not observed: {:?}",
            report.excludes
        );
    }

    #[test]
    fn a_root_on_an_absent_volume_is_unknown() {
        let holder = holder("custody-absent");
        let facts = holder.fixture.facts();
        facts.present.set(false);
        let faults = Faults::new();
        let mounts = holder.fixture.mounts();
        let verdict = take_custody(
            &faults,
            &facts,
            &mounts,
            &holder.fixture.observer(),
            &holder.fixture.state,
            &holder.unit,
            &[],
            CustodyBounds::default(),
        );
        assert_eq!(verdict.name(), "unknown");
    }

    // ---- the real observer, on a fixture root under scratch ------------------

    #[test]
    fn the_calling_process_own_handles_do_not_block_safe() {
        let holder = real_holder("custody-real-self");
        let payload = holder.payload();
        // The handles custody itself holds while it scans: a directory
        // descriptor on the root and an open file inside it. Before the
        // correction these were the evidence that refused every delete.
        let root_handle = std::fs::File::open(holder.root_path()).expect("the root opens");
        let file_handle = std::fs::File::open(&payload).expect("the payload opens");
        let observer = TableObserver::new(vec![self_identity()]);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.self_excluded, std::process::id());
        assert_eq!(evidence.holds, 0);
        assert_eq!(evidence.foreign_uid_processes, 0);
        // The handles were open across the scan, not closed before it.
        assert!(root_handle.metadata().is_ok());
        assert!(file_handle.metadata().is_ok());
    }

    #[test]
    fn another_same_uid_process_holding_the_root_open_is_unsafe() {
        let holder = real_holder("custody-real-other");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        let observer = TableObserver::new(vec![self_identity(), child.identity()]);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unsafe", "{:?}", verdict.reason());
        assert!(
            verdict
                .reason()
                .unwrap_or_default()
                .contains(&child.process.id().to_string()),
            "the refusal names the other holder: {:?}",
            verdict.reason()
        );
    }

    #[test]
    fn removing_the_self_exclusion_refuses_on_the_calling_process() {
        let holder = real_holder("custody-real-counterfactual");
        let payload = holder.payload();
        let handle = std::fs::File::open(&payload).expect("the payload opens");
        let observer = NoSelfExclusion {
            inner: platform(),
            table: vec![self_identity()],
        };
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unsafe", "{:?}", verdict.reason());
        assert!(
            verdict
                .reason()
                .unwrap_or_default()
                .contains(&std::process::id().to_string()),
            "without the exclusion the caller's own descriptor refuses it: {:?}",
            verdict.reason()
        );
        assert!(handle.metadata().is_ok());
    }

    #[test]
    fn a_self_start_identity_that_mismatches_is_unknown() {
        let holder = real_holder("custody-real-mismatch");
        holder.payload();
        let observer = MismatchedSelf {
            inner: platform(),
            table: vec![self_identity()],
        };
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict
            .reason()
            .unwrap_or_default()
            .contains("start identity"));
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn a_forced_process_bound_is_unknown() {
        let holder = real_holder("custody-real-bound");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        let observer = TableObserver::new(vec![self_identity(), child.identity()]);
        let bounds = CustodyBounds {
            processes: 0,
            ..CustodyBounds::default()
        };
        let verdict = holder.take_with(&[], &observer, bounds);
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict.reason().unwrap_or_default().contains("bound"));
    }

    #[test]
    fn a_self_exclusion_the_table_never_showed_is_unknown() {
        let holder = real_holder("custody-real-unseen");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        // The calling process is not in the table at all, so the scan cannot
        // prove what it left out and must refuse.
        let observer = TableObserver::new(vec![child.identity()]);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict
            .reason()
            .unwrap_or_default()
            .contains("not observed"));
    }

    #[test]
    fn an_injected_foreign_effective_uid_is_reported_and_does_not_block() {
        let holder = real_holder("custody-real-foreign-injected");
        holder.payload();
        // Two processes of another effective uid, injected as the kernel's
        // table would report them. They are classified and counted; nothing
        // about them is inspected, so no descriptor read can refuse them.
        let observer = TableObserver::new(vec![
            self_identity(),
            ProcessIdentity {
                pid: 4_242,
                effective_uid: Some(0),
                start: 7,
            },
            ProcessIdentity {
                pid: 4_243,
                effective_uid: Some(248),
                start: 8,
            },
        ]);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        assert_eq!(
            verdict
                .evidence()
                .expect("the evidence")
                .foreign_uid_processes,
            2
        );
    }

    #[test]
    fn a_table_entry_with_an_unreadable_effective_uid_is_unknown() {
        let holder = real_holder("custody-real-uid-unreadable");
        holder.payload();
        let observer = TableObserver::new(vec![
            self_identity(),
            ProcessIdentity {
                pid: 4_244,
                effective_uid: None,
                start: 9,
            },
        ]);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict
            .reason()
            .unwrap_or_default()
            .contains("effective uid"));
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn a_process_this_account_may_not_read_is_reclassified_foreign_by_its_identity() {
        let holder = real_holder("custody-real-descriptors-unreadable");
        holder.payload();
        // A pid whose descriptors the kernel will not show this account, claimed
        // by the table as this account's own. The listing answers `EPERM`, and
        // the re-observation reads the pid's kernel identity, which reports
        // another account: it is the named blind spot, counted and reported,
        // rather than an unreadable same-uid process (barge#012, ruling
        // bcaa83d8). Nothing about it is inspected.
        let foreign = a_foreign_identity();
        let observer = TableObserver::new(vec![
            self_identity(),
            ProcessIdentity {
                pid: foreign.pid,
                effective_uid: Some(platform().effective_uid().expect("the account uid")),
                start: foreign.start,
            },
        ]);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.foreign_uid_processes, 1);
        assert_eq!(evidence.retry.retried_unreadable, 1);
        assert_eq!(
            evidence
                .retry
                .retried
                .first()
                .expect("the retried pid")
                .resolution,
            RetryResolution::Foreign
        );
    }

    /// One real process of another effective uid, read from the kernel's own
    /// table: a read-only observation, naming nothing and changing nothing.
    fn a_foreign_identity() -> ProcessIdentity {
        let uid = platform().effective_uid().expect("the account uid");
        let rows = platform()
            .observed_processes()
            .expect("the process table reads")
            .expect("the process table is within its bound");
        rows.into_iter()
            .map(|(identity, _)| identity)
            .find(|identity| identity.effective_uid != Some(uid))
            .expect("this host reports processes of other effective uids")
    }

    /// The real login processes of this host, read-only from the kernel's own
    /// table. They are root-owned on every macOS host that has one; if this host
    /// has none, any process of another effective uid stands in for them, and
    /// the fact under test is the same.
    fn real_foreign_identities() -> Vec<ProcessIdentity> {
        let uid = platform().effective_uid().expect("the account uid");
        let rows = platform()
            .observed_processes()
            .expect("the process table reads")
            .expect("the process table is within its bound");
        let login: Vec<ProcessIdentity> = rows
            .iter()
            .filter(|(identity, command)| command == "login" && identity.effective_uid != Some(uid))
            .map(|(identity, _)| *identity)
            .collect();
        if !login.is_empty() {
            return login;
        }
        rows.iter()
            .filter(|(identity, _)| identity.effective_uid != Some(uid))
            .map(|(identity, _)| *identity)
            .take(1)
            .collect()
    }

    #[test]
    fn real_login_processes_are_reported_as_foreign_and_do_not_block() {
        let holder = real_holder("custody-real-login");
        holder.payload();
        let foreign = real_foreign_identities();
        assert!(
            !foreign.is_empty(),
            "this host reports at least one process of another effective uid"
        );
        // A read-only observation of the real login processes: their identities
        // come from the kernel's table and nothing about them is inspected.
        let mut table = vec![self_identity()];
        table.extend(foreign.iter().copied());
        let observer = TableObserver::new(table);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.foreign_uid_processes, foreign.len());
        assert_eq!(evidence.self_excluded, std::process::id());
    }

    #[test]
    fn without_effective_uid_classification_the_real_login_process_refuses() {
        let uid = platform().effective_uid().expect("the account uid");
        let candidates = real_foreign_identities();
        assert!(!candidates.is_empty(), "there is a process to classify");
        // Identification through the process's own BSD facts is what the scan
        // did before this correction, and it is refused for exactly these
        // processes: that refusal is the counterfactual.
        for identity in &candidates {
            let error = platform()
                .process_start(identity.pid)
                .expect_err("this account may not read that process's BSD facts");
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{}", identity.pid);
            // The kernel's own table classified it without asking the process,
            // which is what makes it the named blind spot instead.

            assert_ne!(identity.effective_uid, Some(uid));
        }
    }

    // ---- barge#012: a transient EPERM is settled only by positive facts ------

    #[test]
    fn a_transient_eperm_settles_to_safe_evidence() {
        let holder = real_holder("custody-012-transient");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        // One `EPERM`, then the kernel's own answer: the retry is what turns the
        // unreadable listing into an observation instead of a refusal.
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.retry.retried_unreadable, 1);
        let record = evidence
            .retry
            .retried
            .first()
            .expect("the retried pid is recorded");
        assert_eq!(record.pid, identity.pid);
        assert_eq!(record.start, identity.start);
        assert_eq!(record.errnos, vec![EPERM]);
        assert_eq!(record.identity_changes, 0);
        assert_eq!(record.resolution, RetryResolution::Listed);
        assert_eq!(record.command.as_deref(), Some("sleep"));
    }

    #[test]
    fn an_eperm_for_a_process_the_kernel_reports_gone_is_resolved_as_gone() {
        let holder = real_holder("custody-012-gone");
        holder.payload();
        // A pid no process has: the kernel's own table reports no such process,
        // which is a positive fact, and a gone process holds no descriptors.
        let absent = ProcessIdentity {
            pid: 4_242_420,
            effective_uid: Some(platform().effective_uid().expect("the account uid")),
            start: 7,
        };
        let observer = TableObserver::stated(
            vec![self_identity(), absent],
            vec![StatedListing {
                pid: absent.pid,
                errnos: vec![EPERM],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.retry.retried_unreadable, 1);
        let record = evidence
            .retry
            .retried
            .first()
            .expect("the retried pid is recorded");
        assert_eq!(record.pid, absent.pid);
        assert_eq!(record.errnos, vec![EPERM]);
        assert_eq!(record.resolution, RetryResolution::Gone);
        assert!(
            record.command.is_none(),
            "the kernel never reported the process, so no command was read: {:?}",
            record.command
        );
    }

    #[test]
    fn an_eperm_with_a_changed_start_identity_re_evaluates_the_new_process() {
        let holder = real_holder("custody-012-restarted");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        let identity = child.identity();
        // The pid now reports a different start identity, so it is a different
        // process: the scan re-evaluates it under the same rule and reads its
        // descriptors, which find the payload holder instead of assuming.
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM],
                start: Some(identity.start.saturating_add(1)),
                uid: None,
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unsafe", "{:?}", verdict.reason());
        assert!(
            verdict
                .reason()
                .unwrap_or_default()
                .contains(&identity.pid.to_string()),
            "the re-evaluated process's descriptor is the refusal: {:?}",
            verdict.reason()
        );
    }

    #[test]
    fn an_eperm_through_every_retry_stays_unknown() {
        let holder = real_holder("custody-012-persistent");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM; 9],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let reason = verdict.reason().unwrap_or_default();
        assert!(reason.contains(&identity.pid.to_string()), "{reason}");
        assert!(reason.contains("retries: 3"), "{reason}");
        assert!(reason.contains("on 4 attempts"), "{reason}");
        // The refusal keeps the bounded record: it is what the next native
        // diagnostic reads, so it survives the failed scan.
        let retry = verdict.retry();
        assert_eq!(retry.retried_unreadable, 1);
        let record = retry.retried.first().expect("the retried pid is recorded");
        assert_eq!(record.pid, identity.pid);
        assert_eq!(record.errnos, vec![EPERM; 4]);
        assert_eq!(record.resolution, RetryResolution::Unreadable);
        let error = verdict.into_evidence().expect_err("unknown refuses");
        assert_eq!(
            error.detail().get("retried_unreadable"),
            Some(&serde_json::Value::from(1u64))
        );
        assert!(
            error
                .detail()
                .get("retried")
                .is_some_and(|records| records.to_string().contains(&identity.pid.to_string())),
            "the refusal's failure detail carries the records: {:?}",
            error.detail()
        );
    }

    #[test]
    fn a_persistent_eperm_records_its_bounded_errno_sequence_in_the_scan_answer() {
        // The bounded sequence the scan records for a pid that stays unreadable,
        // read where it is structured: on the scan's own answer, which is what
        // the refusal is built from.
        let request = DescriptorScanRequest {
            root: std::path::Path::new("/nonexistent"),
            root_device: crate::os::DeviceId::new(1),
            root_inode: 1,
            max_processes: 8,
            max_descriptors: 64,
            timeout: Duration::from_secs(5),
            self_exclusion: None,
        };
        let identity = self_identity();
        let mut faults = vec![StatedListing {
            pid: identity.pid,
            errnos: vec![EPERM; 9],
            ..StatedListing::default()
        }];
        let answer = platform()
            .scan_open_descriptors_with(&request, &[identity], retry_schedule(), &mut faults)
            .expect("an unreadable same-uid process is an answer, not a scan failure");
        let DescriptorScan::Unreadable { reason, retry } = answer else {
            panic!("a persistent EPERM is unreadable, not {answer:?}");
        };
        assert!(reason.contains("retries: 3"), "{reason}");
        assert!(reason.contains("on 4 attempts"), "{reason}");
        assert_eq!(retry.retried_unreadable, 1);
        let record = retry.retried.first().expect("the retried pid is recorded");
        assert_eq!(record.pid, identity.pid);
        assert_eq!(record.start, identity.start);
        assert_eq!(record.errnos, vec![EPERM; 4]);
        assert_eq!(record.resolution, RetryResolution::Unreadable);
    }

    #[test]
    fn an_eperm_then_esrch_listing_is_resolved_as_gone() {
        // The brief's second sequence: the first listing is unreadable, the retry
        // is answered "no such process". The kernel's identity still reports the
        // live process, so the gone answer is the listing's own positive fact.
        let holder = real_holder("custody-012-esrch");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM, ESRCH],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.retry.retried_unreadable, 1);
        let record = evidence
            .retry
            .retried
            .first()
            .expect("the retried pid is recorded");
        assert_eq!(record.pid, identity.pid);
        assert_eq!(record.errnos, vec![EPERM, ESRCH]);
        assert_eq!(record.resolution, RetryResolution::Gone);
    }

    #[test]
    fn an_unreadable_listing_that_is_not_eperm_refuses_at_once() {
        // The retry rule is about `EPERM` alone. Any other reason a listing could
        // not be read is unknown immediately, with no retry and no listing after
        // it, exactly as it was before barge#012.
        let holder = real_holder("custody-012-not-eperm");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EACCES],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict
            .reason()
            .unwrap_or_default()
            .contains("descriptors of pid"));
        assert_eq!(
            verdict.retry().retried_unreadable,
            0,
            "no `EPERM` was re-observed, so there is no retry record"
        );
        assert!(verdict.into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn an_eperm_then_another_errno_records_both_in_the_sequence() {
        // A retry that is answered with some other unreadable reason refuses at
        // once, and the terminal errno is part of the sequence beside the
        // `EPERM` that was retried: that is what tells a diagnostic where the
        // re-observation ended.
        let holder = real_holder("custody-012-terminal-errno");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM, EACCES],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let retry = verdict.retry();
        assert_eq!(retry.retried_unreadable, 1);
        let record = retry.retried.first().expect("the retried pid is recorded");
        assert_eq!(record.errnos, vec![EPERM, EACCES]);
        assert!(verdict.into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn an_eperm_with_a_changed_effective_uid_is_the_named_foreign_blind_spot() {
        let holder = real_holder("custody-012-foreign");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let foreign = platform()
            .effective_uid()
            .expect("the account uid")
            .saturating_add(1);
        // The pid now reports another account's effective uid, so it is the
        // named blind spot (Tom, 2026-10-26, ruling bcaa83d8): counted and
        // reported as classified, never inspected, and therefore not a process
        // this scan observed.
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM],
                start: None,
                uid: Some(foreign),
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.foreign_uid_processes, 1);
        assert_eq!(evidence.retry.retried_unreadable, 1);
        assert_eq!(evidence.processes, 0, "it was classified, never inspected");
        let record = evidence
            .retry
            .retried
            .first()
            .expect("the retried pid is recorded");
        assert_eq!(record.resolution, RetryResolution::Foreign);
    }

    #[test]
    fn the_settle_window_stays_inside_the_scans_time_bound() {
        let holder = real_holder("custody-012-window");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM; 9],
                ..StatedListing::default()
            }],
        );
        // A scan time bound shorter than the first backoff: no retry can be made
        // inside it, so the scan answers bounded rather than retrying past it.
        let bounds = CustodyBounds {
            timeout: Duration::from_millis(20),
            ..CustodyBounds::default()
        };
        let verdict = holder.take_with(&[], &observer, bounds);
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict.reason().unwrap_or_default().contains("bound"));
        // The record of the pid that was mid-re-observation when the bound
        // stopped the scan is not discarded with the bounded answer.
        let retry = verdict.retry();
        assert_eq!(retry.retried_unreadable, 1);
        let record = retry.retried.first().expect("the retried pid is recorded");
        assert_eq!(record.pid, identity.pid);
        assert_eq!(record.resolution, RetryResolution::Bounded);
    }

    #[test]
    fn without_the_retry_the_transient_eperm_refuses() {
        // The counterfactual: the same one-`EPERM`-then-success fixture the retry
        // above settles to `Safe` refuses at once with the schedule removed.
        let holder = real_holder("custody-012-counterfactual");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = TableObserver::without_retries(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                errnos: vec![EPERM],
                ..StatedListing::default()
            }],
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let reason = verdict.reason().unwrap_or_default();
        assert!(reason.contains("retries: 0"), "{reason}");
        let record = verdict
            .retry()
            .retried
            .first()
            .expect("the retried pid is recorded on the refusal");
        assert_eq!(record.errnos, vec![EPERM]);
        assert_eq!(record.resolution, RetryResolution::Unreadable);
        assert!(verdict.into_evidence().is_err(), "unknown refuses");
    }

    // ---- barge#013: a verified platform binary's hidden descriptors ----------

    /// The stated platform answer for one child: this account owns no
    /// Apple-signed platform binary, so the flag that makes hidden descriptors an
    /// OS blind spot is stated instead of read.
    fn stated_platform(identity: ProcessIdentity, platform: StatedPlatform) -> TableObserver {
        TableObserver::stated(
            vec![self_identity(), identity],
            vec![StatedListing {
                pid: identity.pid,
                opaque: 1,
                platform: Some(platform),
                ..StatedListing::default()
            }],
        )
    }

    #[test]
    fn a_verified_platform_binarys_hidden_descriptors_are_an_os_blind_spot() {
        let holder = real_holder("custody-013-platform");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        let identity = child.identity();
        // The kernel hides this child's only vnode descriptor, and the stated
        // code-signing status verifies the process as a platform binary: the
        // descriptor is an OS blind spot, counted and recorded, not a refusal.
        let observer = stated_platform(
            identity,
            StatedPlatform::PlatformBinary {
                command: "sleep".to_owned(),
            },
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "safe", "{:?}", verdict.reason());
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.os_blind_spot.processes, 1);
        assert_eq!(evidence.os_blind_spot.opaque_descriptors, 1);
        let record = evidence
            .os_blind_spot
            .records
            .first()
            .expect("the blind-spot process is recorded");
        assert_eq!(record.pid, identity.pid);
        assert_eq!(record.start, identity.start);
        assert_eq!(record.command, "sleep");
        assert_eq!(record.opaque_descriptors, 1);
        assert_eq!(evidence.retry.retried_unreadable, 0);
    }

    #[test]
    fn a_non_platform_process_with_hidden_descriptors_is_unknown() {
        let holder = real_holder("custody-013-not-platform");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        let identity = child.identity();
        // The same hidden descriptor, on a process whose code-signing status does
        // not carry the platform flag: nothing is exempt and nothing is recorded.
        let observer = stated_platform(
            identity,
            StatedPlatform::NotPlatformBinary {
                command: "sleep".to_owned(),
            },
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let reason = verdict.reason().unwrap_or_default();
        assert!(
            reason.contains("not a verified platform binary"),
            "{reason}"
        );
        assert!(reason.contains("platform binary flag"), "{reason}");
        assert_eq!(
            verdict.retry().retried_unreadable,
            0,
            "the listing was readable, so there is no `EPERM` retry record"
        );
        assert!(verdict.into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn a_failed_code_signing_read_is_unknown() {
        let holder = real_holder("custody-013-csops-failed");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        let observer = stated_platform(identity, StatedPlatform::Failed { errno: EPERM });
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let reason = verdict.reason().unwrap_or_default();
        assert!(
            reason.contains("code-signing status is unreadable"),
            "{reason}"
        );
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn a_start_identity_that_changes_around_the_code_signing_read_is_unknown() {
        let holder = real_holder("custody-013-identity-changed");
        holder.payload();
        let child = Child::idle();
        let identity = child.identity();
        // The pid names a different process after the status read, so nothing
        // that read says is evidence about the descriptors just listed, even
        // though the flag was reported.
        let observer = stated_platform(
            identity,
            StatedPlatform::IdentityChanged {
                command: "sleep".to_owned(),
                start: identity.start.saturating_add(1),
            },
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let reason = verdict.reason().unwrap_or_default();
        assert!(reason.contains("start identity"), "{reason}");
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn a_platform_binary_with_a_readable_descriptor_inside_the_root_is_unsafe() {
        let holder = real_holder("custody-013-platform-open");
        let first = holder.payload();
        let second = holder.root_path().join("second.bin");
        std::fs::write(&second, b"second").expect("the second payload is written");
        let child = Child::holding_two(&first, &second);
        let identity = child.identity();
        // One of the child's vnode descriptors is hidden and the process is a
        // verified platform binary, but the other is readable and inside the
        // root: the exemption never covers a descriptor the scan can place.
        let observer = stated_platform(
            identity,
            StatedPlatform::PlatformBinary {
                command: "sleep".to_owned(),
            },
        );
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unsafe", "{:?}", verdict.reason());
        assert!(
            verdict
                .reason()
                .unwrap_or_default()
                .contains(&identity.pid.to_string()),
            "the readable descriptor of the platform process refuses it: {:?}",
            verdict.reason()
        );
    }

    #[test]
    fn without_the_platform_flag_the_same_fixture_refuses() {
        // The counterfactual to the blind-spot case: the same process, the same
        // hidden descriptor and the same scan, with only the code-signing answer
        // different. The flag is what buys the exemption, and without it the
        // hidden descriptor refuses instead of being recorded.
        let holder = real_holder("custody-013-counterfactual");
        let payload = holder.payload();
        let child = Child::holding(&payload);
        let identity = child.identity();
        let verified = holder.take_with(
            &[],
            &stated_platform(
                identity,
                StatedPlatform::PlatformBinary {
                    command: "sleep".to_owned(),
                },
            ),
            CustodyBounds::default(),
        );
        assert_eq!(verified.name(), "safe", "{:?}", verified.reason());
        let refused = holder.take_with(
            &[],
            &stated_platform(
                identity,
                StatedPlatform::NotPlatformBinary {
                    command: "sleep".to_owned(),
                },
            ),
            CustodyBounds::default(),
        );
        assert_eq!(refused.name(), "unknown", "{:?}", refused.reason());
        assert!(refused.into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn more_blind_spot_processes_than_the_record_bound_refuses() {
        // The evidence never drops a blind-spot process. A scan that meets more of
        // them than its record holds refuses, rather than report a `Safe` verdict
        // that counts processes it no longer names.
        let holder = real_holder("custody-013-blind-spot-bound");
        holder.payload();
        let children: Vec<Child> = (0..crate::os::BLIND_SPOT_RECORDS.saturating_add(1))
            .map(|_| Child::idle())
            .collect();
        let mut table = vec![self_identity()];
        let mut faults = Vec::new();
        for child in &children {
            let identity = child.identity();
            table.push(identity);
            faults.push(StatedListing {
                pid: identity.pid,
                opaque: 1,
                platform: Some(StatedPlatform::PlatformBinary {
                    command: "sleep".to_owned(),
                }),
                ..StatedListing::default()
            });
        }
        let observer = TableObserver::stated(table, faults);
        let verdict = holder.take_with(&[], &observer, CustodyBounds::default());
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        let reason = verdict.reason().unwrap_or_default();
        assert!(
            reason.contains("more than 32 platform-verified processes"),
            "the refusal names the record bound: {reason}"
        );
        assert!(reason.contains("did not fit"), "{reason}");
        assert!(verdict.into_evidence().is_err(), "unknown refuses");
    }

    #[test]
    fn a_real_platform_binary_is_verified_and_this_test_binary_is_not() {
        // A read-only observation of this host: at least one same-uid process is
        // an Apple platform binary according to its own code-signing status, and
        // a locally built binary is not.
        let uid = platform().effective_uid().expect("the account uid");
        let rows = platform()
            .observed_processes()
            .expect("the process table reads")
            .expect("the process table is within its bound");
        let verified = rows
            .iter()
            .filter(|(identity, _)| identity.effective_uid == Some(uid))
            .find_map(
                |(identity, _)| match platform().verify_platform_binary(*identity) {
                    Ok(PlatformStatus::PlatformBinary { command }) => Some((*identity, command)),
                    _ => None,
                },
            );
        let (identity, command) = verified.expect("this host runs a same-uid platform binary");
        assert_eq!(Some(uid), identity.effective_uid);
        assert!(!command.is_empty(), "the kernel reports its command");
        let own = platform()
            .verify_platform_binary(self_identity())
            .expect("this test binary's code-signing status reads");
        assert!(
            matches!(own, PlatformStatus::NotVerified { .. }),
            "a locally built test binary is not a platform binary: {own:?}"
        );
    }
}
