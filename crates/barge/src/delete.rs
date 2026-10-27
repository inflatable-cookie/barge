//! Whole-unit deletion: the intent journal, the quarantine rename, Finder
//! tolerance, `ENOTEMPTY` retries and replay (stage 1 design, "Writer custody and
//! safe deletion" and "Whole-unit deletion").
//!
//! This is the only code in Barge that may delete, and it is a library
//! capability: no CLI verb deletes and this slice wires no scheduled effect. A
//! deletion acts on one [`DeletionUnit`] - a directory or a file, inside an area
//! folder, with an optional marker of a recognised family and the grant that
//! authorises it - and it is whole-unit only, descriptor-relative and bounded.
//! Barge's own roots are the `kind: directory, marker: barge` case of that same
//! unit, so S4's sweep has one delete path and none of its own.
//!
//! Each phase is durable at its own step, before the effect it authorises and
//! after the effect it records:
//!
//! 1. `intent` before the quarantine rename, recording the unit (kind, name,
//!    parent, expected identity, marker filename and family, the planned private
//!    capture name and the grant), the plan digest and the time;
//! 2. `renamed` before the first unlink;
//! 3. `marker_last` after the unit's marker - when it records one - is unlinked
//!    last and before the quarantine leaf is removed;
//! 4. `removed` after Barge's own final unlink of the verified captured leaf
//!    succeeded, which is the durable completion evidence;
//! 5. `deleted` once the leaf is gone, which settles the intent.
//!
//! A stop after the first effect leaves the intent pending and the quarantine
//! leaf in place; a later replay resolves that intent only from its recorded
//! unit identity and marker bytes, and never infers that a deletion completed.
//! A symlink inside the quarantine stops the deletion and is never followed, and
//! an entry that is neither a plain file of this user on this device, a
//! directory, nor Finder metadata stops it too. No rule ever widens what is
//! deleted.
//!
//! Every entry is removed through one rule: **capture, verify, record, unlink**.
//! The name is exclusively renamed to `.barge-del-<random 128-bit nonce>` in the
//! same directory, the captured object's own facts are re-read under that private
//! name and compared with the observation the walk took of it, the capture and its
//! verdict are published into the intent, and only then is the private name
//! unlinked. A mismatch never unlinks: it is published as a `foreign_capture`
//! **before** anything puts the object back, and then the object goes back under
//! its original name, or, when that name is taken, stays under the private name
//! with both names reported. That closes the check-then-unlink-by-name race, in
//! which a same-uid racer replaces each entry after its check and the next check
//! passes. A file unit goes through exactly that rule at its own level: there is
//! no unlink by name anywhere in this module.
//!
//! The quarantine leaf itself is removed the same way: the visible
//! `.barge-prune-<intent-id>` name is captured to a private name and its kind,
//! device and inode are checked against the intent before it is removed, so a
//! swap in the unit's parent can never turn the final removal into the removal of
//! a foreign object. For a directory the leaf is walked first; for a file the
//! captured leaf is unlinked.
//!
//! The durable facts that carry the rule across a stop - so an absent name is never
//! proof that Barge deleted its object (stage 1 design, "Durable custody of
//! captured objects") - are:
//!
//! - a `.barge-del-*` name is unlinked only while the intent records its capture,
//!   its verdict and its device, inode and kind. A replay may re-verify a
//!   **recorded, verified** capture - the same device, inode and kind, through the
//!   descriptor it verified - and then remove it, which is how a nested capture a
//!   stop left behind is finished. A name recorded in `foreign_captures`, an
//!   unrecorded name, a name whose identity no longer matches and an unverifiable
//!   name are never unlinked: they stay reported and keep the intent pending;
//! - a capture whose verification failed is recorded in `foreign_captures` before
//!   its put-back, so a replay never mistakes the object for content of the unit;
//! - a capture the intent records whose private name is gone is `Unknown`: the
//!   record proves the capture happened, not that Barge unlinked it, so a replay
//!   reports it and keeps the intent pending instead of completing past it. The
//!   quarantine leaf is the one exception: its own record's absence is decided by
//!   the phase, because only the durable `removed` phase proves Barge removed the
//!   leaf;
//! - a captured directory whose owner cannot write or search it has its original
//!   permission mode recorded before the mode is changed, a scope guard restores it
//!   on every return, `?` error and panic, and a replay restores recorded modes
//!   first - before it reads the marker or decides the phase;
//! - the `removed` phase is published after Barge's own final unlink and before the
//!   intent settles, and an absent leaf without it stays unknown and pending.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::area::{self, AreaDeclaration, AreaDocument, MarkerExpectation};
use crate::contract::{INTENT_ROWS, LISTING_NAME_ROWS};
use crate::custody::{self, CustodyBounds, CustodyEvidence, PendingIntent};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    ChildRemoval, DeviceId, DirHandle, DirOps, EntryFacts, EntryKind, MountRoot, ProcessObserver,
    RemoveKind, RenameOutcome, RetryEvidence, VolumeFacts, VolumeUuid,
};
use crate::registry;
use crate::state::{
    self, is_capture_name, CaptureRecord, CaptureVerdict, CapturedObject, ForeignCapture,
    IntentPhase, IntentRecord, IntentUnit, IntentWrite, LockSet, PutBackOutcome, StatePaths,
    CAPTURE_PREFIX, FOREIGN_REASON_BYTES, QUARANTINE_PREFIX,
};
use crate::unit::{self, DeletionUnit, MarkerFamily, UnitKind};

/// How deep a whole-root delete walks (design, "Bounded resources").
pub const DELETE_DEPTH: usize = 12;

/// How many names one directory may hold before the delete stops.
pub const DELETE_NAMES: usize = LISTING_NAME_ROWS;

/// How long one root's deletion may take.
pub const DELETE_TIMEOUT: Duration = Duration::from_secs(120);

/// How many times an `ENOTEMPTY` removal is attempted.
pub const ENOTEMPTY_ATTEMPTS: usize = 3;

/// The pause between `ENOTEMPTY` attempts: Finder can recreate `.DS_Store`.
pub const ENOTEMPTY_PAUSE: Duration = Duration::from_secs(1);

/// After how many examined entries the volume is re-verified.
const REVERIFY_ENTRIES: usize = 512;

/// How many fresh private names one capture tries before it stops: a collision
/// on a 128-bit random name is a defect or a racer, never an ordinary retry.
const CAPTURE_ATTEMPTS: usize = 3;

/// The size bound Finder metadata must stay inside to be removed by class.
///
/// A `.DS_Store` or `._name` file that appeared after the classification has no
/// recorded inode to verify against, so it is verified by class instead: a
/// regular file of this user on this device, no larger than this. Anything
/// bigger is not tolerated as Finder metadata and stops the deletion.
const FINDER_METADATA_BYTES: u64 = 1 << 20;

/// One deletion's bounds. A test shortens them; a real run uses the design's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteBounds {
    /// The maximum walk depth.
    pub depth: usize,
    /// The maximum names one directory may hold.
    pub names: usize,
    /// How long the whole deletion may take.
    pub timeout: Duration,
    /// How many times an `ENOTEMPTY` removal is attempted.
    pub retry_attempts: usize,
    /// The pause between those attempts.
    pub retry_pause: Duration,
    /// The custody bounds the deletion's verdict ran within.
    pub custody: CustodyBounds,
}

impl Default for DeleteBounds {
    fn default() -> Self {
        Self {
            depth: DELETE_DEPTH,
            names: DELETE_NAMES,
            timeout: DELETE_TIMEOUT,
            retry_attempts: ENOTEMPTY_ATTEMPTS,
            retry_pause: ENOTEMPTY_PAUSE,
            custody: CustodyBounds::default(),
        }
    }
}

/// How one deletion ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteOutcome {
    /// The whole root is gone and the intent is settled.
    Deleted,
    /// The deletion stopped before completion; the intent is pending and the
    /// quarantine leaf is kept.
    Pending,
    /// A replay found the root still in place, so it rolled the intent back and
    /// left the root alone.
    RolledBack,
}

impl DeleteOutcome {
    /// The outcome's name, as a document reports it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Pending => "pending",
            Self::RolledBack => "rolled_back",
        }
    }
}

/// What one whole-unit deletion did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteReport {
    /// The verified volume's UUID.
    pub volume_uuid: String,
    /// The unit the deletion acted on.
    pub unit_name: String,
    /// The unit's kind: `directory` or `file`.
    pub unit_kind: String,
    /// The parent directory the unit lived in, relative to its area folder.
    pub parent: String,
    /// The intent's id, which named the quarantine leaf.
    pub intent_id: String,
    /// The plan digest the deletion was authorised by.
    pub plan_digest: String,
    /// The phase the intent reached.
    pub phase: IntentPhase,
    /// How the deletion ended.
    pub outcome: DeleteOutcome,
    /// Why it stopped short of completion, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The quarantine leaf's path, kept while the intent is pending.
    pub quarantine: PathBuf,
    /// How many entries were removed, the marker included.
    pub entries_removed: usize,
    /// How many directories were removed, the leaf included.
    pub directories_removed: usize,
    /// Whether the unit's marker - when it records one - was unlinked last.
    pub marker_removed: bool,
    /// The custody evidence that authorised the first effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custody: Option<CustodyEvidence>,
    /// The bounded `EPERM` re-observation record of the custody verdict that
    /// refused after the intent was written, when one did: a pending deletion
    /// keeps that evidence too, so a diagnostic of the refusal reads it from the
    /// report rather than losing it to a reason string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custody_retry: Option<RetryEvidence>,
}

impl DeleteReport {
    /// Whether the deletion completed and the intent settled.
    pub const fn is_deleted(&self) -> bool {
        matches!(self.outcome, DeleteOutcome::Deleted)
    }

    /// Whether the deletion left a pending intent behind.
    pub const fn is_pending(&self) -> bool {
        matches!(self.outcome, DeleteOutcome::Pending)
    }
}

/// One pending intent a replay did not settle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRow {
    /// The intent's id.
    pub intent_id: String,
    /// The unit the intent acts on.
    pub unit_name: String,
    /// The unit's kind: `directory` or `file`.
    pub unit_kind: String,
    /// The phase it reached.
    pub phase: IntentPhase,
    /// Why it is still pending.
    pub reason: String,
}

/// What one replay pass found and did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayReport {
    /// How many intents the journal held.
    pub examined: usize,
    /// How many were settled by the both-absent proof.
    pub settled: usize,
    /// How many were rolled back because the root was still in place.
    pub rolled_back: usize,
    /// How many resumed a deletion and completed it.
    pub resumed: usize,
    /// The intents that stay pending, with their reasons.
    pub pending: Vec<PendingRow>,
    /// How many entries the resumed deletions removed.
    pub entries_removed: usize,
}

impl ReplayReport {
    /// Whether nothing is left pending.
    pub fn is_settled(&self) -> bool {
        self.pending.is_empty()
    }
}

/// How one walk step ended.
enum Walk {
    /// Nothing stop-worthy was seen.
    Clean,
    /// The deletion must stop; the reason names what it saw.
    Stop(String),
}

/// How one directory removal ended.
enum Removal {
    /// The directory is gone.
    Gone,
    /// The directory survived its attempts.
    Stopped(String),
}

/// The counts one deletion accumulated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    entries: usize,
    directories: usize,
    marker_removed: bool,
}

/// Delete one deletion unit whole, on a registered volume.
///
/// Every refusal the unit record, the registry, the volume, the declaration and
/// custody can cause happens before the first effect. Once the intent is
/// published, a stop leaves it pending and returns a report instead of pretending
/// the unit is gone; the caller (S4's expiry or S7's `maintain`) reports that and
/// exits `3`.
#[allow(clippy::too_many_arguments)]
pub fn delete_unit_on_volume(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
    unit: &DeletionUnit,
    bounds: DeleteBounds,
) -> Result<DeleteReport> {
    delete_unit_authorized(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        declaration,
        stored,
        unit,
        bounds,
        None,
    )
}

type EligibilityCheck<'a> = dyn Fn(&registry::VolumeRecord) -> Result<()> + 'a;

#[allow(clippy::too_many_arguments)]
pub(crate) fn delete_unit_authorized(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
    unit: &DeletionUnit,
    bounds: DeleteBounds,
    authorize: Option<&EligibilityCheck<'_>>,
) -> Result<DeleteReport> {
    // The unit's own record is validated first: its parent, name, kind, marker,
    // grant and plan digest each have one well-formed shape, and the grant must
    // be the form its marker family needs.
    unit.validate()?;
    let volume_uuid = unit.volume_uuid.clone();

    // Pre-flight, effect-free: an invalid registry, an unregistered or
    // undeclared volume, a mismatched or absent volume, a declaration that does
    // not contain its folder, and a unit that is not in the folder the
    // configuration resolves all refuse before a directory or lock exists.
    let record = area::registered_volume(state_paths, &volume_uuid)?;
    record.declared_durability()?;
    let volume = area::prepare(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        &volume_uuid,
        declaration,
        stored,
    )?;
    let resolved = &volume.resolved;
    if unit.root_dir != resolved.root_dir || unit.folder != resolved.folder {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the unit is not in the folder the configuration resolves".to_owned(),
        )
        .with_detail("unit_root_dir", unit.root_dir.clone().into())
        .with_detail("configured_root_dir", resolved.root_dir.clone().into())
        .with_detail("unit_folder", unit.folder.clone().into())
        .with_detail("configured_folder", resolved.folder.clone().into()));
    }
    if !resolved.exists() {
        return Err(Error::new(
            ErrorKind::VolumeAbsent,
            format!("the area folder {} is not there", resolved.path.display()),
        )
        .with_detail("path", resolved.path.display().to_string().into()));
    }
    let area_handle = dirs.open_path(&resolved.path)?;
    let Some(parent) = unit.open_parent(dirs, &area_handle)? else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the unit's parent is not there, so there is nothing to delete at {}",
                unit.path_in(&resolved.path).display()
            ),
        ));
    };
    let Some(present) = dirs.child_facts(&parent, &unit.name)? else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the unit {} is not there, so there is nothing to delete",
                unit.path_in(&resolved.path).display()
            ),
        ));
    };
    if present.kind != unit.entry_kind() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the unit {} is a {}, not the {} the plan recorded",
                unit.path_in(&resolved.path).display(),
                present.kind.as_str(),
                unit.kind.as_str()
            ),
        ));
    }

    state_paths.initialize()?;
    let mut locks = LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(&volume_uuid)?;
    let _unit_lock = lock_deletion_unit(&mut locks, unit)?;

    // The registry is re-read under the locks before the intent is written, so a
    // record that changed while the plan ran is refused rather than trusted.
    let record = area::registered_volume(state_paths, &volume_uuid)?;
    record.declared_durability()?;
    if let Some(authorize) = authorize {
        authorize(&record)?;
    }
    let Some((area_path, area_handle)) = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &unit.root_dir,
        &unit.folder,
    )?
    else {
        return Err(Error::new(
            ErrorKind::VolumeAbsent,
            "the area folder vanished before the intent was written".to_owned(),
        ));
    };
    let Some(parent_handle) = unit.open_parent(dirs, &area_handle)? else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the unit's parent vanished before the intent was written".to_owned(),
        ));
    };
    let parent_path = if unit.parent.is_empty() {
        area_path.clone()
    } else {
        area_path.join(&unit.parent)
    };

    // Custody is decided before anything durable is written.
    let intents = state::read_intents(state_paths, INTENT_ROWS)?;
    let pending = PendingIntent::pending(&intents.records);
    let evidence = custody::take_custody(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        unit,
        &pending,
        bounds.custody,
    )
    .into_evidence()?;

    let intent_id = hex(&dirs.random_bytes(16)?);
    let capture_name = format!("{QUARANTINE_PREFIX}{intent_id}");
    let quarantine = parent_path.join(&capture_name);
    let now = state::now_ms();
    let mut intent = IntentRecord {
        schema: crate::contract::INTENT_SCHEMA.to_owned(),
        intent_id: intent_id.clone(),
        volume_uuid: unit.volume_uuid.clone(),
        mount_hint: resolved.mount_point.clone(),
        root_dir: unit.root_dir.clone(),
        folder: unit.folder.clone(),
        unit: IntentUnit {
            parent: unit.parent.clone(),
            name: unit.name.clone(),
            kind: unit.kind,
            device: unit.device.get(),
            inode: unit.inode,
            marker: unit.marker.clone(),
            capture_name,
            grant: unit.grant.clone(),
        },
        plan_digest: unit.plan_digest.clone(),
        phase: IntentPhase::Intent,
        generation: 1,
        recorded_ms: now,
        phase_ms: now,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
    };
    let write = locks.begin_intent_write()?;
    state::publish_intent(&write, state_paths, &intent)?;

    // The effect may not start on a custody verdict that has aged: it is
    // re-decided after the intent publish and before the first destructive step.
    let after_publish = custody::take_custody(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        unit,
        &pending,
        bounds.custody,
    );
    if !after_publish.is_safe() {
        let mut report = pending_report(
            &intent,
            quarantine,
            Counts::default(),
            format!(
                "custody is not proven after the intent was written: {}",
                after_publish.reason().unwrap_or("the verdict is not safe")
            ),
            Some(evidence),
        );
        report.custody_retry = Some(after_publish.retry().clone());
        return Ok(report);
    }

    // The quarantine rename is atomic and exclusive inside the unit's parent: an
    // existing name refuses instead of being replaced, and a cross-device or
    // changed-inode result stops before anything is deleted.
    match dirs.rename_child(&parent_handle, &unit.name, intent.capture_name()) {
        Ok(RenameOutcome::Renamed) => {}
        Ok(RenameOutcome::TargetExists) => {
            return Ok(pending_report(
                &intent,
                quarantine,
                Counts::default(),
                format!(
                    "the private capture name `{}` is already taken, so the unit was not renamed",
                    intent.capture_name()
                ),
                Some(evidence),
            ))
        }
        Err(error) => {
            return Ok(pending_report(
                &intent,
                quarantine,
                Counts::default(),
                format!("the quarantine rename did not happen: {error}"),
                Some(evidence),
            ))
        }
    }
    if let Some(reason) = renamed_leaf_mismatch(dirs, &parent_handle, &intent) {
        return Ok(pending_report(
            &intent,
            quarantine,
            Counts::default(),
            reason,
            Some(evidence),
        ));
    }
    intent = state::advance_intent(&write, state_paths, &intent, IntentPhase::Renamed)?;

    let mut report = finish_deletion(
        &write,
        state_paths,
        dirs,
        facts,
        observer,
        &intent,
        &parent_path,
        &parent_handle,
        bounds,
    );
    report.custody = Some(evidence);
    Ok(report)
}

/// Take the lock of the unit being deleted: the root lock for a Barge root id,
/// which is the lock a hold on that root takes too, and the unit's own derived
/// key otherwise.
fn lock_deletion_unit(locks: &mut LockSet, unit: &DeletionUnit) -> Result<crate::state::StateLock> {
    if state::is_identifier(&unit.name) {
        return locks.lock_root(&unit.name);
    }
    locks.lock_unit(&state::unit_lock_key(
        &unit.volume_uuid,
        &unit.parent,
        &unit.name,
    ))
}

/// What one renamed quarantine leaf is: absent, or not the object the intent
/// recorded. `None` means it is exactly the recorded unit under its private name.
fn renamed_leaf_mismatch(
    dirs: &dyn DirOps,
    parent: &DirHandle,
    intent: &IntentRecord,
) -> Option<String> {
    let name = intent.capture_name();
    match intent.unit.kind {
        UnitKind::Directory => match dirs.open_child_dir(parent, name) {
            Ok(leaf) => {
                let opened = leaf.identity();
                if opened.device.get() == intent.unit.device && opened.inode == intent.unit.inode {
                    None
                } else {
                    Some(format!(
                        "the renamed leaf is device {} inode {}, not the recorded {} / {}",
                        opened.device, opened.inode, intent.unit.device, intent.unit.inode
                    ))
                }
            }
            Err(error) => Some(format!("the renamed leaf cannot be opened: {error}")),
        },
        UnitKind::File => match dirs.child_facts(parent, name) {
            Ok(Some(facts))
                if facts.kind == EntryKind::File
                    && facts.device.get() == intent.unit.device
                    && facts.inode == intent.unit.inode =>
            {
                None
            }
            Ok(Some(facts)) => Some(format!(
                "the renamed leaf is a {} on device {} inode {}, not the recorded file on device {} inode {}",
                facts.kind.as_str(),
                facts.device,
                facts.inode,
                intent.unit.device,
                intent.unit.inode
            )),
            Ok(None) => Some("the renamed leaf is not there".to_owned()),
            Err(error) => Some(format!("the renamed leaf cannot be read: {error}")),
        },
    }
}

/// Delete one quarantined unit whole, from the phase the intent reached.
#[allow(clippy::too_many_arguments)]
fn finish_deletion(
    write: &IntentWrite,
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    observer: &dyn ProcessObserver,
    intent: &IntentRecord,
    parent_path: &Path,
    parent: &DirHandle,
    bounds: DeleteBounds,
) -> DeleteReport {
    let quarantine = parent_path.join(intent.capture_name());
    let uid = match observer.effective_uid() {
        Ok(uid) => uid,
        Err(error) => {
            return pending_report(
                intent,
                quarantine,
                Counts::default(),
                format!("the account uid is unreadable: {error}"),
                None,
            )
        }
    };
    let leaf_facts = match dirs.child_facts(parent, intent.capture_name()) {
        Ok(Some(facts)) => facts,
        Ok(None) => {
            return pending_report(
                intent,
                quarantine,
                Counts::default(),
                "the captured leaf is not there".to_owned(),
                None,
            )
        }
        Err(error) => {
            return pending_report(
                intent,
                quarantine,
                Counts::default(),
                format!("the captured leaf cannot be read: {error}"),
                None,
            )
        }
    };
    if leaf_facts.kind != intent.unit.kind.entry_kind()
        || leaf_facts.device.get() != intent.unit.device
        || leaf_facts.inode != intent.unit.inode
    {
        return pending_report(
            intent,
            quarantine,
            Counts::default(),
            format!(
                "the captured leaf is a {} on device {} inode {}, not the recorded {} on device {} inode {}",
                leaf_facts.kind.as_str(),
                leaf_facts.device,
                leaf_facts.inode,
                intent.unit.kind.as_str(),
                intent.unit.device,
                intent.unit.inode
            ),
            None,
        );
    }
    if leaf_facts.uid != uid {
        return pending_report(
            intent,
            quarantine,
            Counts::default(),
            format!(
                "the captured leaf belongs to uid {}, not this account's {uid}",
                leaf_facts.uid
            ),
            None,
        );
    }
    // A directory is opened, so the walk runs through the descriptor whose
    // identity was compared with the record. A file has no contents to walk.
    let leaf = if intent.unit.kind == UnitKind::Directory {
        match dirs.open_child_dir(parent, intent.capture_name()) {
            Ok(leaf) => {
                let opened = leaf.identity();
                if opened.device.get() != intent.unit.device || opened.inode != intent.unit.inode {
                    return pending_report(
                        intent,
                        quarantine,
                        Counts::default(),
                        format!(
                            "the captured leaf opened as device {} inode {}, not the recorded {} / {}",
                            opened.device, opened.inode, intent.unit.device, intent.unit.inode
                        ),
                        None,
                    );
                }
                Some(leaf)
            }
            Err(error) => {
                return pending_report(
                    intent,
                    quarantine,
                    Counts::default(),
                    format!("the captured leaf cannot be opened: {error}"),
                    None,
                )
            }
        }
    } else {
        None
    };

    let mut counts = Counts::default();
    let mut current = intent.clone();

    // While the phase is below `marker_last`, the unit's marker must still be the
    // bytes the intent recorded - or, for a unit that records none, the leaf must
    // carry none of the recognised families.
    if current.phase < IntentPhase::MarkerLast {
        match (&current.unit.marker, &leaf) {
            (Some(marker), Some(leaf)) => {
                match leaf_marker_digest(dirs, observer, intent, leaf, &marker.filename) {
                    Ok(MarkerBytes::Digest(digest)) if digest == marker.digest => {}
                    Ok(_) => {
                        return pending_report(
                            intent,
                            quarantine,
                            counts,
                            "the captured leaf's marker is not the recorded marker bytes"
                                .to_owned(),
                            None,
                        )
                    }
                    Err(error) => {
                        return pending_report(
                            intent,
                            quarantine,
                            counts,
                            format!("the captured leaf's marker is unreadable: {error}"),
                            None,
                        )
                    }
                }
            }
            (Some(_), None) => {
                return pending_report(
                    intent,
                    quarantine,
                    counts,
                    "the unit records a marker but the captured leaf is not a directory".to_owned(),
                    None,
                )
            }
            (None, Some(leaf)) => match markerless_leaf_check(dirs, leaf) {
                Ok(None) => {}
                Ok(Some(reason)) => {
                    return pending_report(intent, quarantine, counts, reason, None)
                }
                Err(error) => {
                    return pending_report(
                        intent,
                        quarantine,
                        counts,
                        format!("the captured leaf's marker files are unreadable: {error}"),
                        None,
                    )
                }
            },
            (None, None) => {}
        }
    }

    let journal = CaptureJournal::new(write, state_paths, &current);
    // The leaf's own mode: a directory whose owner cannot write into it cannot
    // have its contents removed. Its mode is published before the change and a
    // guard puts it back on every abort, and a replay restores it first.
    //
    // The chain starts from the captures the intent already records: a stopped run
    // may have left one under its private name, and its record is what authorises
    // finishing it. The leaf's own mode record, when this run takes one, goes at
    // the front, because it is the outermost private name.
    let mut base_chain: Vec<CaptureRecord> = intent.captures.clone();
    let mut leaf_custody_mode: Option<u32> = None;
    let leaf_custody = match &leaf {
        Some(leaf) => match dirs.dir_facts(leaf) {
            Ok(leaf_facts)
                if leaf_facts.kind == EntryKind::Directory
                    && leaf_facts.permission_mode() & 0o300 != 0o300 =>
            {
                base_chain.retain(|held| held.private != intent.capture_name());
                base_chain.insert(
                    0,
                    CaptureRecord {
                        private: intent.capture_name().to_owned(),
                        original: intent.capture_name().to_owned(),
                        device: intent.unit.device,
                        inode: intent.unit.inode,
                        kind: "directory".to_owned(),
                        verdict: CaptureVerdict::Observed,
                        mode: Some(leaf_facts.st_mode),
                    },
                );
                if let Err(error) = journal.record_captures(&base_chain) {
                    return pending_report(
                        &journal.intent(),
                        quarantine,
                        counts,
                        format!("the captured leaf's mode could not be recorded: {error}"),
                        None,
                    );
                }
                if let Err(error) = dirs.set_dir_mode(leaf, leaf_facts.permission_mode() | 0o300) {
                    return pending_report(
                        &journal.intent(),
                        quarantine,
                        counts,
                        format!("the captured leaf's recorded mode could not be applied: {error}"),
                        None,
                    );
                }
                leaf_custody_mode = Some(leaf_facts.st_mode);
                Some(ModeCustody::new(dirs, leaf, leaf_facts.st_mode))
            }
            Ok(_) => None,
            Err(error) => {
                return pending_report(
                    &journal.intent(),
                    quarantine,
                    counts,
                    format!("the opened captured leaf cannot be read: {error}"),
                    None,
                )
            }
        },
        None => None,
    };

    let mut walk = match WalkState::new(
        dirs,
        facts,
        observer,
        intent.volume_uuid.clone(),
        DeviceId::new(intent.unit.device),
        intent.mount_hint.clone(),
        bounds,
        &journal,
        base_chain,
        intent
            .unit
            .marker
            .as_ref()
            .map(|marker| marker.filename.clone()),
    ) {
        Ok(walk) => walk,
        Err(error) => {
            return pending_report(
                intent,
                quarantine,
                counts,
                format!("the deletion walk cannot start: {error}"),
                None,
            )
        }
    };

    // A device change before the first unlink stops the deletion.
    if let Err(error) = walk.verify_volume() {
        return pending_report(
            intent,
            quarantine,
            counts,
            format!("the volume no longer verifies before the first unlink: {error}"),
            None,
        );
    }

    if current.phase < IntentPhase::MarkerLast {
        if let Some(leaf) = &leaf {
            let classified = match walk.classify(leaf, 0, Path::new("")) {
                Ok(outcome) => outcome,
                Err(error) => {
                    return pending_report(
                        intent,
                        quarantine,
                        counts,
                        format!("the deletion walk stopped: {error}"),
                        None,
                    )
                }
            };
            if let Walk::Stop(reason) = classified {
                return pending_report(&journal.intent(), quarantine, counts, reason, None);
            }
            match walk.delete_contents(leaf, 0, Path::new(""), true, &mut counts) {
                Ok(Walk::Stop(reason)) => {
                    return pending_report(&journal.intent(), quarantine, counts, reason, None)
                }
                Ok(Walk::Clean) => {}
                Err(error) => {
                    return pending_report(
                        intent,
                        quarantine,
                        counts,
                        format!("the deletion walk stopped: {error}"),
                        None,
                    )
                }
            }
            // The unit's marker, when it records one, is the ownership proof and
            // is captured and verified like every other entry before it is
            // unlinked last.
            if current.unit.marker.is_some() {
                match walk.remove_marker_last(leaf, &mut counts) {
                    Ok(Walk::Clean) => {}
                    Ok(Walk::Stop(reason)) => {
                        return pending_report(&journal.intent(), quarantine, counts, reason, None)
                    }
                    Err(error) => {
                        return pending_report(
                            intent,
                            quarantine,
                            counts,
                            format!("the marker could not be removed: {error}"),
                            None,
                        )
                    }
                }
            }
        }
        // A file unit has no contents and no marker: its own identity, checked
        // against the record above, is the whole unit, so the phase moves
        // straight to `marker_last` and the captured leaf is removed next.
        match state::advance_intent(
            write,
            state_paths,
            &journal.intent(),
            IntentPhase::MarkerLast,
        ) {
            Ok(next) => {
                current = next;
                journal.advanced(current.clone());
            }
            Err(error) => {
                return pending_report(
                    &current,
                    quarantine,
                    counts,
                    format!("the marker-last phase could not be recorded: {error}"),
                    None,
                )
            }
        }
    }

    // The marker is gone: only now may the captured leaf be removed, and it is
    // captured and verified again like every other entry, so the visible name in
    // the unit's parent can never turn the final removal into the removal of a
    // foreign object.
    match walk.remove_leaf(parent, &current, leaf_custody_mode, &mut counts) {
        Ok(Removal::Gone) => {}
        Ok(Removal::Stopped(reason)) => {
            return pending_report(&journal.intent(), quarantine, counts, reason, None)
        }
        Err(error) => {
            return pending_report(
                &journal.intent(),
                quarantine,
                counts,
                format!("the captured leaf could not be removed: {error}"),
                None,
            )
        }
    }
    if current.unit.kind == UnitKind::Directory {
        counts.directories = counts.directories.saturating_add(1);
    }
    if let Some(custody) = &leaf_custody {
        custody.settled();
    }
    // The durable completion evidence: Barge's own final unlink of a verified
    // captured private name succeeded. A replay may settle the intent only after
    // this phase; without it, an absence is unknown, never a completion.
    match state::advance_intent(write, state_paths, &journal.intent(), IntentPhase::Removed) {
        Ok(removed) => {
            current = removed.clone();
            journal.advanced(removed);
        }
        Err(error) => {
            return pending_report(
                &journal.intent(),
                quarantine,
                counts,
                format!("the removed phase could not be recorded: {error}"),
                None,
            )
        }
    }
    match state::advance_intent(write, state_paths, &current, IntentPhase::Deleted) {
        Ok(settled) => DeleteReport {
            volume_uuid: settled.volume_uuid.to_string(),
            unit_name: settled.unit.name.clone(),
            unit_kind: settled.unit.kind.as_str().to_owned(),
            parent: settled.unit.parent.clone(),
            intent_id: settled.intent_id.clone(),
            plan_digest: settled.plan_digest.clone(),
            phase: settled.phase,
            outcome: DeleteOutcome::Deleted,
            reason: None,
            quarantine,
            entries_removed: counts.entries,
            directories_removed: counts.directories,
            marker_removed: counts.marker_removed,
            custody: None,
            custody_retry: None,
        },
        Err(error) => pending_report(
            &current,
            quarantine,
            counts,
            format!("the deleted phase could not be recorded: {error}"),
            None,
        ),
    }
}

/// A markerless directory unit's captured leaf must carry none of the recognised
/// marker families: a unit that became marked is not the unit the plan named.
fn markerless_leaf_check(dirs: &dyn DirOps, leaf: &DirHandle) -> Result<Option<String>> {
    for filename in unit::RECOGNISED_MARKER_FILES {
        if dirs.child_facts(leaf, filename)?.is_some() {
            return Ok(Some(format!(
                "the captured leaf now carries `{filename}`, so it is not the unmarked unit the plan named"
            )));
        }
    }
    Ok(None)
}

/// What one captured leaf's marker is: absent, or the digest of the bytes that
/// are there. A marker that is there but unreadable is an error, never "absent",
/// so a deletion never mistakes an unknown marker for a removed one.
enum MarkerBytes {
    Absent,
    Digest(String),
}

/// Read one captured leaf's marker digest, from the filename the intent recorded.
///
/// A Barge marker is read by [`area::read_marker_with_digest`], so it must still
/// be a `barge.root/1` document; a foreign family's marker is read as the bytes
/// at its own pinned filename. The marker records the unit's creation path, which
/// the rename did not change, so the expectation is only what lets the bytes be
/// read; the caller compares the digest.
fn leaf_marker_digest(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    intent: &IntentRecord,
    leaf: &DirHandle,
    filename: &str,
) -> Result<MarkerBytes> {
    let expectation = MarkerExpectation {
        volume_uuid: &intent.volume_uuid,
        resolved_path: Path::new(""),
        effective_uid: observer.effective_uid()?,
        now_ms: state::now_ms(),
    };
    let Some(family) = MarkerFamily::from_marker_file(filename) else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{filename}` is not a marker filename Barge reads"),
        ));
    };
    if family == MarkerFamily::Barge {
        let (marker, digest) = area::read_marker_with_digest(dirs, leaf, &expectation);
        return match marker {
            crate::area::MarkerState::Absent => Ok(MarkerBytes::Absent),
            _ => match digest {
                Some(digest) => Ok(MarkerBytes::Digest(digest)),
                None => Err(Error::new(
                    ErrorKind::UnknownState,
                    "the captured leaf's marker bytes could not be read".to_owned(),
                )),
            },
        };
    }
    let Some(facts) = dirs.child_facts(leaf, filename)? else {
        return Ok(MarkerBytes::Absent);
    };
    if facts.kind != EntryKind::File || facts.uid != expectation.effective_uid {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the captured leaf's `{filename}` is a {} of uid {}, not this account's regular file",
                facts.kind.as_str(),
                facts.uid
            ),
        ));
    }
    let cap = crate::contract::MARKER_CAP_BYTES;
    if facts.size > cap as u64 {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("the captured leaf's `{filename}` is over the {cap}-byte cap"),
        ));
    }
    match dirs.read_child_file(leaf, filename, cap)? {
        Some(bytes) => Ok(MarkerBytes::Digest(crate::digest::sha256_hex(&bytes))),
        None => Ok(MarkerBytes::Absent),
    }
}

fn pending_report(
    intent: &IntentRecord,
    quarantine: PathBuf,
    counts: Counts,
    reason: String,
    custody: Option<CustodyEvidence>,
) -> DeleteReport {
    DeleteReport {
        volume_uuid: intent.volume_uuid.to_string(),
        unit_name: intent.unit.name.clone(),
        unit_kind: intent.unit.kind.as_str().to_owned(),
        parent: intent.unit.parent.clone(),
        intent_id: intent.intent_id.clone(),
        plan_digest: intent.plan_digest.clone(),
        phase: intent.phase,
        outcome: DeleteOutcome::Pending,
        reason: Some(reason),
        quarantine,
        entries_removed: counts.entries,
        directories_removed: counts.directories,
        marker_removed: counts.marker_removed,
        custody,
        custody_retry: None,
    }
}

/// The intent journal a deletion walk publishes into.
///
/// Two durable facts make the walk safe, and both are published **before** the
/// effect they protect:
///
/// - the in-flight captures, with their verdict and any mode Barge is about to
///   change, so a private name is only ever unlinked while the intent records its
///   capture and a replay re-verifies the same identity;
/// - every capture whose verification failed, before its put-back, because a
///   put-back returns an object Barge never observed to a name a later walk would
///   treat as an ordinary entry.
///
/// A record that cannot be written therefore stops the deletion with the object
/// still under its private name, which no run and no replay ever unlinks.
struct CaptureJournal<'a> {
    write: &'a IntentWrite,
    state_paths: &'a StatePaths,
    intent: RefCell<IntentRecord>,
}

impl<'a> CaptureJournal<'a> {
    /// The journal the walk publishes into.
    fn new(
        write: &'a IntentWrite,
        state_paths: &'a StatePaths,
        intent: &IntentRecord,
    ) -> CaptureJournal<'a> {
        Self {
            write,
            state_paths,
            intent: RefCell::new(intent.clone()),
        }
    }

    /// Publish one capture whose verification failed, before its put-back.
    ///
    /// A later record of the same capture replaces the earlier one, so the
    /// put-back answer is added rather than duplicated.
    fn record_foreign(&self, foreign: ForeignCapture) -> Result<()> {
        let next = {
            let current = self.intent.borrow();
            state::record_foreign_capture(self.write, self.state_paths, &current, &foreign)?
        };
        *self.intent.borrow_mut() = next;
        Ok(())
    }

    /// Publish the in-flight captures durably.
    ///
    /// This is what makes a private name deletable: a `.barge-del-*` name is
    /// unlinked only while the intent records its capture and verification, with
    /// the mode Barge is about to change, so a replay re-verifies the same object.
    fn record_captures(&self, captures: &[CaptureRecord]) -> Result<()> {
        let next = {
            let current = self.intent.borrow();
            state::record_captures(self.write, self.state_paths, &current, captures)?
        };
        *self.intent.borrow_mut() = next;
        Ok(())
    }

    /// The intent as the journal has it now.
    fn intent(&self) -> IntentRecord {
        self.intent.borrow().clone()
    }

    /// Take a phase the caller published through the journal's own token, so a
    /// later report from the journal names the phase that is actually durable.
    fn advanced(&self, intent: IntentRecord) {
        *self.intent.borrow_mut() = intent;
    }
}

/// One deletion walk: the bounds, the accumulated facts and the volume
/// re-verification that a long walk needs.
struct WalkState<'a> {
    dirs: &'a dyn DirOps,
    facts: &'a dyn VolumeFacts,
    volume_uuid: VolumeUuid,
    device: DeviceId,
    mount: PathBuf,
    uid: u32,
    bounds: DeleteBounds,
    deadline: Instant,
    examined: usize,
    /// Every relative path the classification pass saw. The delete pass removes
    /// what was classified and Finder metadata that appeared again; anything
    /// else is a new entry, which stops the deletion rather than widening it.
    expected: HashSet<PathBuf>,
    /// The journal every capture is published into before it is unlinked, so the
    /// record is durable even if the process stops immediately afterwards.
    journal: &'a CaptureJournal<'a>,
    /// The captures currently in flight, outermost first. It is the durable list a
    /// private name must appear in before anything unlinks it, and it is bounded
    /// by the depth bound.
    chain: Vec<CaptureRecord>,
    /// The unit's own marker filename, when it records one. It is skipped at the
    /// unit's top level until `remove_marker_last` removes it last, and any other
    /// recognised family's marker there stops the deletion: a unit that became
    /// marked is not the unit the plan named.
    marker: Option<String>,
}

impl<'a> WalkState<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        dirs: &'a dyn DirOps,
        facts: &'a dyn VolumeFacts,
        observer: &'a dyn ProcessObserver,
        volume_uuid: VolumeUuid,
        device: DeviceId,
        mount: PathBuf,
        bounds: DeleteBounds,
        journal: &'a CaptureJournal<'a>,
        base_chain: Vec<CaptureRecord>,
        marker: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            dirs,
            facts,
            volume_uuid,
            device,
            mount,
            uid: observer.effective_uid()?,
            bounds,
            deadline: Instant::now() + bounds.timeout,
            examined: 0,
            expected: HashSet::new(),
            journal,
            chain: base_chain,
            marker,
        })
    }

    /// What a recognised marker at the unit's top level means: the unit's own
    /// marker is skipped until it is removed last, and any other stops the
    /// deletion, because the unit that carries it is not the unit the plan named.
    fn top_level_marker(&self, name: &str) -> Option<Walk> {
        MarkerFamily::from_marker_file(name)?;
        if self.marker.as_deref() == Some(name) {
            return None;
        }
        Some(Walk::Stop(format!(
            "`{name}` appeared inside the unit after the plan, so the unit is not the one the plan named"
        )))
    }

    /// Publish the in-flight captures, which is the durable list a private name
    /// must be in before anything unlinks it.
    fn publish_chain(&self) -> Result<()> {
        self.journal.record_captures(&self.chain)
    }

    /// Whether `facts` is the object one recorded capture names.
    fn matches_capture(record: &CaptureRecord, facts: &EntryFacts) -> bool {
        facts.device.get() == record.device
            && facts.inode == record.inode
            && facts.kind.as_str() == record.kind
    }

    /// The stop for a private name this operation did not record and verify.
    ///
    /// A `.barge-del-*` name is deletable only while the intent records its capture
    /// and verification with the same identity. Anything else - an unrecorded name,
    /// a record whose identity no longer matches, or a name recorded as a foreign
    /// capture - is unverifiable: it is never unlinked, it is reported, and the
    /// intent stays pending.
    fn unrecorded_stop(&self, private: &str, facts: Option<&EntryFacts>) -> String {
        let recorded = self.journal.intent();
        if let Some(foreign) = recorded
            .foreign_captures
            .iter()
            .find(|foreign| foreign.private == private)
        {
            return format!(
                "`{private}` holds the foreign capture of `{}`, whose put back {}: {}; it is never unlinked, so the intent stays pending",
                foreign.original,
                match foreign.put_back {
                    PutBackOutcome::Pending => "was not recorded",
                    PutBackOutcome::Renamed => "put it back under its own name",
                    PutBackOutcome::Taken => "was refused because the name was taken",
                    PutBackOutcome::Failed => "failed",
                },
                foreign.reason
            );
        }
        match recorded
            .captures
            .iter()
            .find(|capture| capture.private == private)
        {
            Some(capture) => format!(
                "`{private}` was recorded as this operation's capture of `{}` ({} on device {} inode {}), but {} is there now, so it is not unlinked and the intent stays pending",
                capture.original,
                capture.kind,
                capture.device,
                capture.inode,
                match facts {
                    Some(facts) => format!(
                        "a {} on device {} inode {}",
                        facts.kind.as_str(),
                        facts.device,
                        facts.inode
                    ),
                    None => "nothing".to_owned(),
                }
            ),
            None => format!(
                "`{private}` is a private capture name this operation did not record, so its object is unverifiable and nothing is unlinked"
            ),
        }
    }

    /// Re-read the volume's facts and refuse when it is no longer the same
    /// volume on the same device.
    fn verify_volume(&self) -> Result<()> {
        let identity = self.facts.identify(&self.mount)?;
        if identity.device != self.device {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "the volume at {} is now device {}, not the recorded {}",
                    self.mount.display(),
                    identity.device,
                    self.device
                ),
            )
            .with_detail("device", identity.device.to_string().into())
            .with_detail("recorded_device", self.device.to_string().into()));
        }
        if identity.volume_uuid.as_ref() != Some(&self.volume_uuid) {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "the volume at {} is no longer {}",
                    self.mount.display(),
                    self.volume_uuid
                ),
            ));
        }
        Ok(())
    }

    /// Account for `added` examined entries: the deadline, the periodic volume
    /// re-verification, and nothing else.
    fn budget(&mut self, added: usize) -> Result<Option<String>> {
        self.examined = self.examined.saturating_add(added);
        if Instant::now() >= self.deadline {
            return Ok(Some(format!(
                "the per-root deadline of {}s was reached after {} entries",
                self.bounds.timeout.as_secs(),
                self.examined
            )));
        }
        if self.examined >= REVERIFY_ENTRIES {
            self.verify_volume()?;
            self.examined = 0;
        }
        Ok(None)
    }

    /// Classify one entry without touching it.
    fn classify_entry(&self, name: &str, facts: &EntryFacts) -> Walk {
        if facts.uid != self.uid {
            return Walk::Stop(format!(
                "`{name}` is owned by uid {}, not this account's {}",
                facts.uid, self.uid
            ));
        }
        if facts.device != self.device {
            return Walk::Stop(format!(
                "`{name}` is on device {}, not the volume's {}",
                facts.device, self.device
            ));
        }
        match facts.kind {
            EntryKind::Directory | EntryKind::File => Walk::Clean,
            EntryKind::Symlink => Walk::Stop(format!(
                "`{name}` is a symbolic link, which this deletion never follows"
            )),
            EntryKind::Other => Walk::Stop(format!("`{name}` is neither a file nor a directory")),
        }
    }

    /// Walk the whole tree once, classifying every entry and changing nothing.
    /// An entry the deletion does not recognise stops it before anything is
    /// unlinked, so the root survives whole.
    fn classify(&mut self, handle: &DirHandle, depth: usize, relative: &Path) -> Result<Walk> {
        if depth > self.bounds.depth {
            return Ok(Walk::Stop(format!(
                "the walk reached its {} level depth bound",
                self.bounds.depth
            )));
        }
        let listing = self.dirs.list_children(handle, self.bounds.names)?;
        if listing.truncated {
            return Ok(Walk::Stop(format!(
                "a directory holds more than {} names",
                self.bounds.names
            )));
        }
        if let Some(stop) = self.budget(listing.entries.len())? {
            return Ok(Walk::Stop(stop));
        }
        for entry in &listing.entries {
            if depth == 0 {
                if let Some(stop) = self.top_level_marker(&entry.name) {
                    return Ok(stop);
                }
                if self.marker.as_deref() == Some(entry.name.as_str()) {
                    continue;
                }
            }
            let facts = self.dirs.child_facts(handle, &entry.name)?;
            if is_capture_name(&entry.name) && facts.is_none() {
                // A recorded capture that answered "not there" is unknown: neither
                // the walk nor a replay may treat it as already removed.
                return Ok(Walk::Stop(self.unrecorded_stop(&entry.name, None)));
            }
            let Some(facts) = facts else {
                // It vanished between the listing and the read.
                continue;
            };
            if let Walk::Stop(reason) = self.classify_entry(&entry.name, &facts) {
                return Ok(Walk::Stop(reason));
            }
            if is_capture_name(&entry.name) {
                // A private name is never an ordinary entry of the tree: it is this
                // operation's own capture under its private name, or it stops the
                // deletion. Its contents are classified as the tree they were, under
                // the captured object's original path.
                let recorded = self.journal.intent();
                let ours = recorded
                    .captures
                    .iter()
                    .find(|capture| {
                        capture.private == entry.name && Self::matches_capture(capture, &facts)
                    })
                    .cloned();
                let Some(capture) = ours else {
                    return Ok(Walk::Stop(self.unrecorded_stop(&entry.name, Some(&facts))));
                };
                if facts.kind == EntryKind::Directory {
                    let original = relative.join(&capture.original);
                    let child = self.dirs.open_child_dir(handle, &entry.name)?;
                    // The recursion is into the recorded capture and nothing else: a
                    // name swapped between the read and the open stops the walk.
                    let opened = child.identity();
                    if opened.device.get() != capture.device || opened.inode != capture.inode {
                        return Ok(Walk::Stop(format!(
                            "`{}` opened as device {} inode {}, not the recorded capture on device {} inode {}, so it is not classified",
                            entry.name, opened.device, opened.inode, capture.device, capture.inode
                        )));
                    }
                    if let Walk::Stop(reason) = self.classify(&child, depth + 1, &original)? {
                        return Ok(Walk::Stop(reason));
                    }
                }
                continue;
            }
            let path = relative.join(&entry.name);
            self.expected.insert(path.clone());
            if facts.kind == EntryKind::Directory {
                let child = self.dirs.open_child_dir(handle, &entry.name)?;
                // Only the directory the classification observed is walked: a name
                // swapped between the read and the open stops the classification,
                // so nothing unverified is ever classified or recorded as expected.
                let opened = child.identity();
                if opened.device != facts.device || opened.inode != facts.inode {
                    return Ok(Walk::Stop(format!(
                        "`{}` opened as device {} inode {}, not the observed {} / {}, so it is not classified",
                        entry.name, opened.device, opened.inode, facts.device, facts.inode
                    )));
                }
                if let Walk::Stop(reason) = self.classify(&child, depth + 1, &path)? {
                    return Ok(Walk::Stop(reason));
                }
            }
        }
        Ok(Walk::Clean)
    }

    /// Capture one child entry under a private random name in its own
    /// directory.
    ///
    /// The rename is the platform's exclusive one, so it either moves the object
    /// the walk asked for or reports that the private name is taken; it never
    /// replaces an entry. A taken private name is retried with fresh random
    /// bytes, so a collision can never be mistaken for a capture.
    fn capture(&mut self, handle: &DirHandle, name: &str) -> Result<Capture> {
        for _ in 0..CAPTURE_ATTEMPTS {
            let private = format!("{CAPTURE_PREFIX}{}", hex(&self.dirs.random_bytes(16)?));
            match self.dirs.rename_child(handle, name, &private)? {
                RenameOutcome::Renamed => return Ok(Capture::Held(private)),
                RenameOutcome::TargetExists => continue,
            }
        }
        Ok(Capture::Stopped(format!(
            "`{name}` could not be captured under a fresh private name: {CAPTURE_ATTEMPTS} were taken"
        )))
    }

    /// Verify one captured object against the observation it was captured from.
    ///
    /// An entry the classification recorded must report the same device, inode and
    /// kind as the observation the walk took, and still belong to this account. A
    /// Finder-named regular file is verified by class instead, whether the
    /// classification recorded it or it was recreated: a regular file of this user
    /// on this device, inside the size bound. The parent binding is re-read through
    /// the descriptor the capture went through, so a volume that changed under it
    /// stops the deletion before any unlink.
    fn verify_capture(
        &mut self,
        handle: &DirHandle,
        private: &str,
        observed: &EntryFacts,
        allowance: Allowance,
    ) -> Result<Verify> {
        let parent = self.dirs.recheck_dir(handle)?;
        let opened = handle.identity();
        if parent.device != self.device || parent.inode != opened.inode {
            return Ok(Verify::Mismatch {
                reason: format!(
                    "the directory holding `{private}` is device {} inode {}, not the opened {} / {} on the recorded device {}",
                    parent.device, parent.inode, opened.device, opened.inode, self.device
                ),
                captured: None,
            });
        }
        let Some(facts) = self.dirs.child_facts(handle, private)? else {
            return Ok(Verify::Mismatch {
                reason: format!("the captured object is not there under `{private}`"),
                captured: None,
            });
        };
        if facts.uid != self.uid {
            return Ok(Verify::Mismatch {
                reason: format!(
                    "the captured `{private}` belongs to uid {}, not this account's {}",
                    facts.uid, self.uid
                ),
                captured: Some(facts),
            });
        }
        let matched = match allowance {
            Allowance::Finder => {
                facts.kind == EntryKind::File
                    && facts.device == self.device
                    && facts.size <= FINDER_METADATA_BYTES
            }
            Allowance::Observed => {
                facts.kind == observed.kind
                    && facts.device == observed.device
                    && facts.inode == observed.inode
            }
        };
        if matched {
            return Ok(Verify::Verified(facts));
        }
        Ok(Verify::Mismatch {
            reason: format!(
                "the captured `{private}` is a {} on device {} inode {} of {} bytes, not the observed {} on device {} inode {}",
                facts.kind.as_str(),
                facts.device,
                facts.inode,
                facts.size,
                observed.kind.as_str(),
                observed.device,
                observed.inode
            ),
            captured: Some(facts),
        })
    }

    /// Capture one entry under a private name, verify the captured object, then
    /// record and unlink it only on a match.
    ///
    /// Nothing is unlinked before it is durably recorded: the captured identity,
    /// the verdict and any mode Barge is about to change are published first, so a
    /// replay re-verifies the same object and can restore the recorded mode. A
    /// mismatch is recorded as a foreign capture **before** anything puts the
    /// object back, and a record that cannot be written means the object is never
    /// put back.
    #[allow(clippy::too_many_arguments)]
    fn capture_and_remove(
        &mut self,
        handle: &DirHandle,
        name: &str,
        observed: &EntryFacts,
        allowance: Allowance,
        depth: usize,
        relative: &Path,
        counts: &mut Counts,
    ) -> Result<Walk> {
        let private = match self.capture(handle, name)? {
            Capture::Held(private) => private,
            Capture::Stopped(reason) => return Ok(Walk::Stop(reason)),
        };
        let captured = match self.verify_capture(handle, &private, observed, allowance)? {
            Verify::Verified(facts) => facts,
            Verify::Mismatch { reason, captured } => {
                let stopped =
                    self.record_foreign(handle, observed, name, &private, &reason, captured)?;
                return Ok(Walk::Stop(stopped));
            }
        };
        let verdict = match allowance {
            Allowance::Finder => CaptureVerdict::Finder,
            Allowance::Observed => CaptureVerdict::Observed,
        };
        let mut record = CaptureRecord {
            private: private.clone(),
            original: name.to_owned(),
            device: captured.device.get(),
            inode: captured.inode,
            kind: captured.kind.as_str().to_owned(),
            verdict,
            mode: None,
        };
        // A directory this account cannot write or search cannot have its contents
        // removed. Its mode is recorded before it is changed, so an abort and a
        // replay put back exactly what was there.
        let changed_mode = if captured.kind == EntryKind::Directory
            && captured.permission_mode() & 0o300 != 0o300
        {
            record.mode = Some(captured.st_mode);
            Some(captured.st_mode)
        } else {
            None
        };
        self.remove_recorded(handle, &record, changed_mode, depth, relative, counts)
    }

    /// Publish one foreign capture durably, put the object back, and report how the
    /// deletion stops.
    ///
    /// The record is written before the put-back, and it is the record that
    /// protects the object: a put-back returns the captured object to an ordinary
    /// name, where a later walk would treat it as an ordinary entry and unlink it.
    fn record_foreign(
        &mut self,
        handle: &DirHandle,
        observed: &EntryFacts,
        name: &str,
        private: &str,
        reason: &str,
        captured: Option<EntryFacts>,
    ) -> Result<String> {
        // `observed` is what the walk expected at the name (the identity it
        // checked), and `captured` is what the capture found instead; both are
        // recorded, so a later run can tell them apart.
        let mut foreign = ForeignCapture {
            private: private.to_owned(),
            original: name.to_owned(),
            device: observed.device.get(),
            inode: observed.inode,
            kind: observed.kind.as_str().to_owned(),
            captured: captured.map(|facts| CapturedObject {
                device: facts.device.get(),
                inode: facts.inode,
                kind: facts.kind.as_str().to_owned(),
            }),
            put_back: PutBackOutcome::Pending,
            reason: bounded_reason(reason),
        };
        if let Err(error) = self.journal.record_foreign(foreign.clone()) {
            // Without the record a put-back would expose the object, so it is not
            // attempted: the object stays under its private name, which no run and
            // no replay ever unlinks, and the intent stays pending.
            return Ok(format!(
                "`{name}` changed after its check and is kept under `{private}` because the mismatch could not be recorded: {error}"
            ));
        }
        // Never unlink. A put-back that itself fails still reports both the
        // original and the private name, because the operator has to decide what
        // happens to the object left under the private one.
        let (put_back, stopped) = match self.put_back(handle, name, private, reason) {
            Ok((put_back, stopped)) => (put_back, stopped),
            Err(error) => (
                PutBack::Failed,
                format!(
                    "`{name}` changed after its check and could not be put back from `{private}`: {error}"
                ),
            ),
        };
        // The answer is evidence, not the protection: the record written before the
        // put-back already stops a replay from resuming, so an update that fails is
        // not fatal. The record keeps saying `pending`, which is true.
        foreign.put_back = match put_back {
            PutBack::Renamed => PutBackOutcome::Renamed,
            PutBack::Taken => PutBackOutcome::Taken,
            PutBack::Failed => PutBackOutcome::Failed,
        };
        let _ = self.journal.record_foreign(foreign);
        Ok(stopped)
    }

    /// Publish one recorded capture, change its mode when the record says so, and
    /// unlink it - or recurse into it and remove it.
    ///
    /// The record is durable before either the chmod or the unlink, so a replay
    /// re-verifies the same object and restores the recorded mode.
    fn remove_recorded(
        &mut self,
        parent: &DirHandle,
        record: &CaptureRecord,
        changed_mode: Option<u32>,
        depth: usize,
        relative: &Path,
        counts: &mut Counts,
    ) -> Result<Walk> {
        // A recorded capture a stopped run left behind is already in the chain,
        // seeded from the intent when the walk started: it keeps its position so
        // the snapshot's outermost-first order is the one a later replay resolves
        // private names in. A capture this run makes is appended.
        match self
            .chain
            .iter()
            .position(|held| held.private == record.private)
        {
            Some(index) => self.chain[index] = record.clone(),
            None => self.chain.push(record.clone()),
        }
        if let Err(error) = self.publish_chain() {
            return Ok(Walk::Stop(format!(
                "the capture of `{}` could not be recorded, so it is not unlinked: {error}",
                record.original
            )));
        }
        let outcome = if record.kind == "file" {
            self.unlink_recorded_file(parent, record, counts)
        } else {
            self.remove_recorded_directory(parent, record, changed_mode, depth, relative, counts)
        };
        // The record leaves the chain only after the removal it authorised
        // succeeded; a stop before that keeps it recorded for a replay.
        if let Some(index) = self
            .chain
            .iter()
            .position(|held| held.private == record.private)
        {
            self.chain.remove(index);
        }
        outcome
    }

    /// Unlink one recorded file under its private name.
    fn unlink_recorded_file(
        &mut self,
        parent: &DirHandle,
        record: &CaptureRecord,
        counts: &mut Counts,
    ) -> Result<Walk> {
        // Re-verify the recorded identity immediately before the unlink: what is
        // removed is the object the record names, never whatever a racer put at
        // the private name since the capture.
        let still_recorded = self
            .dirs
            .child_facts(parent, &record.private)?
            .is_some_and(|facts| Self::matches_capture(record, &facts));
        if !still_recorded {
            return Ok(Walk::Stop(format!(
                "`{}` is no longer the recorded capture of `{}`, so it is not unlinked and the intent stays pending",
                record.private, record.original
            )));
        }
        match self
            .dirs
            .remove_child(parent, &record.private, RemoveKind::File)?
        {
            ChildRemoval::Removed => {
                counts.entries = counts.entries.saturating_add(1);
                Ok(Walk::Clean)
            }
            ChildRemoval::Absent => Ok(Walk::Stop(format!(
                "`{}` vanished before Barge unlinked it, so another process may have moved the captured object; the intent stays pending",
                record.private
            ))),
            ChildRemoval::NotEmpty => Ok(Walk::Stop(format!(
                "`{}` reported not-empty as a file",
                record.private
            ))),
        }
    }

    /// Remove one recorded directory: open it through its private name, change its
    /// recorded mode when it has to, delete its contents through that descriptor,
    /// and only then remove the private name.
    fn remove_recorded_directory(
        &mut self,
        parent: &DirHandle,
        record: &CaptureRecord,
        changed_mode: Option<u32>,
        depth: usize,
        relative: &Path,
        counts: &mut Counts,
    ) -> Result<Walk> {
        let child = match self.dirs.open_child_dir(parent, &record.private) {
            Ok(child) => child,
            Err(error) => {
                return Ok(Walk::Stop(format!(
                    "the captured `{}` could not be opened: {error}",
                    record.private
                )))
            }
        };
        let opened = child.identity();
        if opened.device.get() != record.device || opened.inode != record.inode {
            return Ok(Walk::Stop(format!(
                "`{}` opened as device {} inode {}, not the recorded {} / {}",
                record.private, opened.device, opened.inode, record.device, record.inode
            )));
        }
        // The recorded mode was published before this change; the guard puts it
        // back on every return, `?` error and panic that aborts or puts back.
        let custody = match changed_mode {
            Some(mode) => match self.dirs.set_dir_mode(&child, (mode & 0o7777) | 0o300) {
                Ok(()) => Some(ModeCustody::new(self.dirs, &child, mode)),
                Err(error) => {
                    return Ok(Walk::Stop(format!(
                        "the recorded mode of `{}` could not be applied, so nothing inside it is removed: {error}",
                        record.private
                    )))
                }
            },
            None => None,
        };
        if let Walk::Stop(reason) =
            self.delete_contents(&child, depth + 1, relative, false, counts)?
        {
            return Ok(Walk::Stop(restore_custody(custody, reason)));
        }
        // The private name can only be replaced once the directory is empty; the
        // recorded inode is what is removed, and nothing else.
        let still_recorded = self
            .dirs
            .child_facts(parent, &record.private)?
            .is_some_and(|facts| Self::matches_capture(record, &facts));
        if !still_recorded {
            return Ok(Walk::Stop(restore_custody(
                custody,
                format!(
                    "`{}` is no longer the recorded capture, so it is not removed",
                    record.private
                ),
            )));
        }
        // The recursion refreshed the chain; make this directory the last recorded
        // entry before its own removal.
        if let Err(error) = self.publish_chain() {
            return Ok(Walk::Stop(restore_custody(
                custody,
                format!(
                    "the capture of `{}` could not be re-recorded before its removal: {error}",
                    record.original
                ),
            )));
        }
        match self.remove_directory(parent, &child, &record.private, depth + 1, relative, counts)? {
            Removal::Gone => {
                if let Some(custody) = &custody {
                    custody.settled();
                }
                counts.directories = counts.directories.saturating_add(1);
                Ok(Walk::Clean)
            }
            Removal::Stopped(reason) => Ok(Walk::Stop(restore_custody(custody, reason))),
        }
    }

    /// Remove the captured leaf itself, through the same rule as every entry.
    ///
    /// The visible private name is captured again and the captured object's kind,
    /// device and inode are checked against the intent before anything is removed,
    /// so a swap in the unit's parent can never turn the final removal into the
    /// removal of a foreign object. A mismatch puts the object back and stops; the
    /// completion phase is recorded only after Barge's own removal of the captured
    /// object succeeded. A directory leaf is walked and removed; a file leaf is
    /// unlinked, under the same record.
    fn remove_leaf(
        &mut self,
        parent: &DirHandle,
        intent: &IntentRecord,
        leaf_mode: Option<u32>,
        counts: &mut Counts,
    ) -> Result<Removal> {
        let private = match self.capture(parent, intent.capture_name())? {
            Capture::Held(private) => private,
            Capture::Stopped(reason) => return Ok(Removal::Stopped(reason)),
        };
        let Some(facts) = self.dirs.child_facts(parent, &private)? else {
            return Ok(Removal::Stopped(format!(
                "`{private}` vanished after its capture, so the captured leaf is not removed and the intent stays pending"
            )));
        };
        let expected_kind = intent.unit.kind.entry_kind();
        if facts.kind != expected_kind
            || facts.device.get() != intent.unit.device
            || facts.inode != intent.unit.inode
        {
            // It is not the recorded leaf: a foreign object at the private name.
            // It is recorded as a foreign capture before anything puts it back, and
            // the intent stays pending.
            let reason = format!(
                "the captured leaf's name held a {} on device {} inode {}, not the recorded {} on device {} inode {}",
                facts.kind.as_str(),
                facts.device,
                facts.inode,
                intent.unit.kind.as_str(),
                intent.unit.device,
                intent.unit.inode
            );
            let mut foreign = ForeignCapture {
                private: private.clone(),
                original: intent.capture_name().to_owned(),
                device: intent.unit.device,
                inode: intent.unit.inode,
                kind: intent.unit.kind.as_str().to_owned(),
                captured: Some(CapturedObject {
                    device: facts.device.get(),
                    inode: facts.inode,
                    kind: facts.kind.as_str().to_owned(),
                }),
                put_back: PutBackOutcome::Pending,
                reason: bounded_reason(&reason),
            };
            if let Err(error) = self.journal.record_foreign(foreign.clone()) {
                return Ok(Removal::Stopped(format!(
                    "`{private}` is not the recorded quarantine leaf and the mismatch could not be recorded, so it stays under its private name: {error}"
                )));
            }
            let (put_back, where_it_is) =
                match self
                    .dirs
                    .rename_child(parent, &private, intent.capture_name())?
                {
                    RenameOutcome::Renamed => (
                        PutBackOutcome::Renamed,
                        format!("it is back under `{}`", intent.capture_name()),
                    ),
                    RenameOutcome::TargetExists => (
                        PutBackOutcome::Taken,
                        format!("it stays under `{private}` because its name is taken"),
                    ),
                };
            foreign.put_back = put_back;
            let _ = self.journal.record_foreign(foreign);
            return Ok(Removal::Stopped(format!(
                "`{private}` is not the recorded quarantine leaf, so nothing is removed and {where_it_is}: {reason}"
            )));
        }
        let record = CaptureRecord {
            private,
            original: intent.capture_name().to_owned(),
            device: intent.unit.device,
            inode: intent.unit.inode,
            kind: intent.unit.kind.as_str().to_owned(),
            verdict: CaptureVerdict::Observed,
            mode: leaf_mode,
        };
        // The leaf is the whole chain now: everything inside it is gone, and the
        // record of the leaf replaces the earlier chain.
        self.chain.clear();
        match self.remove_recorded(parent, &record, leaf_mode, 0, Path::new(""), counts)? {
            Walk::Clean => Ok(Removal::Gone),
            Walk::Stop(reason) => Ok(Removal::Stopped(reason)),
        }
    }

    /// Resume one recorded capture the walk found under its private name.
    ///
    /// It is this operation's own captured object, recorded and verified before the
    /// stop: the same identity is re-verified and the recorded mode is applied
    /// again where it was changed, so deleting it is the continuation of the same
    /// capture, not a new decision about an ordinary entry.
    fn resume_recorded(
        &mut self,
        parent: &DirHandle,
        record: CaptureRecord,
        depth: usize,
        relative: &Path,
        counts: &mut Counts,
    ) -> Result<Walk> {
        let changed_mode = record.mode;
        self.remove_recorded(parent, &record, changed_mode, depth, relative, counts)
    }

    /// Put a captured object back under its original name after a failed
    /// verification, and report how the deletion stops.
    ///
    /// Nothing is unlinked on this path. When the original name is taken the object
    /// stays under its private name and both names are reported, because an
    /// operator has to decide which of the two should exist; that is the case the
    /// caller records as a foreign capture.
    fn put_back(
        &mut self,
        handle: &DirHandle,
        name: &str,
        private: &str,
        reason: &str,
    ) -> Result<(PutBack, String)> {
        match self.dirs.rename_child(handle, private, name)? {
            RenameOutcome::Renamed => Ok((
                PutBack::Renamed,
                format!(
                    "`{name}` changed after its check, so it was put back under its own name and nothing was unlinked: {reason}"
                ),
            )),
            RenameOutcome::TargetExists => Ok((
                PutBack::Taken,
                format!(
                    "`{name}` changed after its check and its original name is taken, so it is kept under `{private}`; both names are kept: {reason}"
                ),
            )),
        }
    }

    /// Remove the marker last, through the same capture, verify and unlink rule
    /// every other entry goes through.
    fn remove_marker_last(&mut self, leaf: &DirHandle, counts: &mut Counts) -> Result<Walk> {
        let Some(marker) = self.marker.clone() else {
            return Ok(Walk::Clean);
        };
        let Some(observed) = self.dirs.child_facts(leaf, &marker)? else {
            return Ok(Walk::Stop(format!(
                "the `{marker}` marker vanished before it was unlinked last"
            )));
        };
        if let Walk::Stop(reason) = self.classify_entry(&marker, &observed) {
            return Ok(Walk::Stop(reason));
        }
        if observed.kind != EntryKind::File {
            return Ok(Walk::Stop(format!(
                "the `{marker}` marker is a {}, not a regular file",
                observed.kind.as_str()
            )));
        }
        match self.capture_and_remove(
            leaf,
            &marker,
            &observed,
            Allowance::Observed,
            0,
            Path::new(""),
            counts,
        )? {
            Walk::Clean => {
                counts.marker_removed = true;
                // The marker's capture is complete. The chain is published without
                // it, so the durable record matches what Barge actually removed: a
                // replay that stops at `marker_last`, before the leaf is captured,
                // then reads no completed capture as a missing one, and resumes the
                // leaf capture instead of reporting an unknown.
                if let Err(error) = self.publish_chain() {
                    return Ok(Walk::Stop(format!(
                        "the marker was unlinked but the completed capture could not be recorded: {error}"
                    )));
                }
                Ok(Walk::Clean)
            }
            stop => Ok(stop),
        }
    }

    /// Delete every entry of one directory except, at the top level, the marker.
    ///
    /// Every entry is captured under a private name and verified before it is
    /// unlinked, so what is removed is what was verified. The pass removes what
    /// the classification recorded, Finder metadata that appeared again (by
    /// class), and a recorded capture of this operation under its private name; a
    /// new entry that is none of those stops the deletion rather than widening it.
    fn delete_contents(
        &mut self,
        handle: &DirHandle,
        depth: usize,
        relative: &Path,
        skip_marker: bool,
        counts: &mut Counts,
    ) -> Result<Walk> {
        if depth > self.bounds.depth {
            return Ok(Walk::Stop(format!(
                "the walk reached its {} level depth bound",
                self.bounds.depth
            )));
        }
        let listing = self.dirs.list_children(handle, self.bounds.names)?;
        if listing.truncated {
            return Ok(Walk::Stop(format!(
                "a directory holds more than {} names",
                self.bounds.names
            )));
        }
        if let Some(stop) = self.budget(listing.entries.len())? {
            return Ok(Walk::Stop(stop));
        }
        for entry in &listing.entries {
            if skip_marker {
                if let Some(stop) = self.top_level_marker(&entry.name) {
                    return Ok(stop);
                }
                if self.marker.as_deref() == Some(entry.name.as_str()) {
                    continue;
                }
            }
            // This read is the observation the capture is verified against.
            let facts = self.dirs.child_facts(handle, &entry.name)?;
            if is_capture_name(&entry.name) && facts.is_none() {
                // The recorded capture's name answered "not there" after the
                // listing: an absence is unknown, never a step that completed.
                return Ok(Walk::Stop(self.unrecorded_stop(&entry.name, None)));
            }
            let Some(facts) = facts else {
                continue;
            };
            if let Walk::Stop(reason) = self.classify_entry(&entry.name, &facts) {
                return Ok(Walk::Stop(reason));
            }
            let path = relative.join(&entry.name);
            if is_capture_name(&entry.name) {
                // Only a recorded, verified capture of this operation may be
                // removed, and only while the object is the recorded one.
                let recorded = self.journal.intent();
                let Some(record) = recorded
                    .captures
                    .iter()
                    .find(|capture| {
                        capture.private == entry.name && Self::matches_capture(capture, &facts)
                    })
                    .cloned()
                else {
                    return Ok(Walk::Stop(self.unrecorded_stop(&entry.name, Some(&facts))));
                };
                // Its contents are the tree they were, so the path the walk
                // expects them under is the captured object's original one.
                let original = relative.join(&record.original);
                if let Walk::Stop(reason) =
                    self.resume_recorded(handle, record, depth, &original, counts)?
                {
                    return Ok(Walk::Stop(reason));
                }
                continue;
            }
            let allowance = if is_finder_metadata(&entry.name) && facts.kind == EntryKind::File {
                // Finder metadata is verified by class whether the classification
                // recorded it or Finder rebuilt it between the observation and the
                // capture: its inode is not a stable identity.
                Allowance::Finder
            } else if self.expected.contains(&path) {
                Allowance::Observed
            } else {
                return Ok(Walk::Stop(format!(
                    "`{}` appeared after the classification, so the delete stops",
                    path.display()
                )));
            };
            if let Walk::Stop(reason) = self.capture_and_remove(
                handle,
                &entry.name,
                &facts,
                allowance,
                depth,
                &path,
                counts,
            )? {
                return Ok(Walk::Stop(reason));
            }
        }
        Ok(Walk::Clean)
    }

    /// Remove one verified directory by its recorded name, retrying `ENOTEMPTY` at
    /// most the declared number of times because Finder can recreate `.DS_Store`
    /// mid-delete. Each retry re-deletes only what the walk already recognises,
    /// through the descriptor of the directory that was verified as the captured
    /// one; it never re-opens the name to widen the walk.
    fn remove_directory(
        &mut self,
        parent: &DirHandle,
        child: &DirHandle,
        name: &str,
        depth: usize,
        relative: &Path,
        counts: &mut Counts,
    ) -> Result<Removal> {
        let attempts = self.bounds.retry_attempts.max(1);
        for attempt in 0..attempts {
            match self
                .dirs
                .remove_child(parent, name, RemoveKind::Directory)?
            {
                ChildRemoval::Removed => return Ok(Removal::Gone),
                ChildRemoval::Absent => {
                    return Ok(Removal::Stopped(format!(
                        "`{name}` vanished before Barge removed it, so another process may have moved the captured object; the intent stays pending"
                    )))
                }
                ChildRemoval::NotEmpty => {
                    if attempt + 1 == attempts {
                        return Ok(Removal::Stopped(format!(
                            "`{name}` was still not empty after {attempts} attempts"
                        )));
                    }
                    std::thread::sleep(self.bounds.retry_pause);
                    if let Walk::Stop(reason) =
                        self.delete_contents(child, depth, relative, false, counts)?
                    {
                        return Ok(Removal::Stopped(reason));
                    }
                }
            }
        }
        Ok(Removal::Stopped(format!(
            "`{name}` was still not empty after {attempts} attempts"
        )))
    }
}

/// One captured directory whose recorded mode Barge changed.
///
/// The recorded mode is published before the change. The guard puts it back on
/// every return, `?` error and panic that aborts or puts back, so an object is
/// never left with a changed mode without the intent recording the original.
struct ModeCustody<'a> {
    dirs: &'a dyn DirOps,
    handle: &'a DirHandle,
    mode: u32,
    settled: Cell<bool>,
}

impl<'a> ModeCustody<'a> {
    /// The guard over one changed mode.
    fn new(dirs: &'a dyn DirOps, handle: &'a DirHandle, mode: u32) -> Self {
        Self {
            dirs,
            handle,
            mode,
            settled: Cell::new(false),
        }
    }

    /// The recorded object is gone, so there is no mode to put back.
    fn settled(&self) {
        self.settled.set(true);
    }

    /// Put the recorded mode back now, and report whether it happened.
    fn restore(&self) -> Result<()> {
        if self.settled.replace(true) {
            return Ok(());
        }
        self.dirs.set_dir_mode(self.handle, self.mode & 0o7777)
    }
}

impl Drop for ModeCustody<'_> {
    fn drop(&mut self) {
        if !self.settled.replace(true) {
            // Every return, `?` error and panic restores the recorded mode. A
            // failure here keeps the intent pending, and the intent keeps the mode
            // recorded, so a replay restores it first.
            let _ = self.dirs.set_dir_mode(self.handle, self.mode & 0o7777);
        }
    }
}

/// Give one captured directory's recorded mode back, folding a failure into the
/// reason the deletion stops with.
fn restore_custody(custody: Option<ModeCustody<'_>>, reason: String) -> String {
    match custody {
        Some(custody) => match custody.restore() {
            Ok(()) => reason,
            Err(error) => format!(
                "{reason}; the recorded mode of the captured directory could not be restored: {error}"
            ),
        },
        None => reason,
    }
}

/// Why one entry may be removed at all.
///
/// A private capture name is never on this list: it is removed only as a recorded
/// capture of this operation, through the record the intent holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Allowance {
    /// The entry the classification pass recorded, verified against the
    /// observation the delete pass took of it.
    Observed,
    /// A Finder-named regular file, verified by class instead of by inode.
    ///
    /// Finder rebuilds `.DS_Store` and `._name` whenever it likes, so such a file
    /// has no stable inode to match: whether the classification recorded it or it
    /// appeared afterwards, what is verified is the class and the size bound.
    Finder,
}

/// One capture's answer.
enum Capture {
    /// The entry is held under this private name.
    Held(String),
    /// The entry could not be captured; the deletion stops.
    Stopped(String),
}

/// One capture verification's answer.
enum Verify {
    /// The captured object is the observed one, or Finder metadata of the allowed
    /// class. It carries the captured facts, which are what the intent records.
    Verified(EntryFacts),
    /// The object is not the observed one, so it is never unlinked. The captured
    /// facts are carried when they could be read, so the record says what was
    /// there instead.
    Mismatch {
        /// Why the capture did not verify.
        reason: String,
        /// The captured object's own facts, when they could be read.
        captured: Option<EntryFacts>,
    },
}

/// What a put-back of one unverified capture answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutBack {
    /// The object is back under its original name.
    Renamed,
    /// The original name is taken, so the object stays under its private name.
    Taken,
    /// The put-back failed, so the object stays under its private name.
    Failed,
}

/// One reason string bounded to what an intent record may hold.
///
/// A reason that would not fit is truncated at a character boundary: the record
/// says what happened in as much detail as it can, and the truncated form is never
/// mistaken for the complete one because the stop's own reason is reported beside
/// it.
fn bounded_reason(reason: &str) -> String {
    if reason.len() <= FOREIGN_REASON_BYTES {
        return reason.to_owned();
    }
    let mut end = FOREIGN_REASON_BYTES;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

/// Finder metadata is exactly Oxide's two forms: `.DS_Store`, or `._` followed
/// by a nonempty name. Nothing broader is tolerated.
fn is_finder_metadata(name: &str) -> bool {
    name == ".DS_Store" || (name.starts_with("._") && name.len() > 2)
}

/// Replay every pending intent, settling only what its recorded identity proves.
///
/// A pending intent is resolved only from its recorded unit and its durable
/// journal. It resumes when the captured leaf is at its visible
/// `.barge-prune-<intent-id>` name with the recorded kind, device and inode and the
/// marker digest matches (`renamed`), or, at `marker_last`, when the leaf is still
/// at that visible name and the unit's marker is positively absent; a resumed walk
/// re-verifies each **recorded, verified** capture and removes only those. It
/// settles only when the leaf is positively absent **and the durable `removed`
/// phase was recorded**, which is the evidence of Barge's own final unlink.
/// Absence alone never resolves an intent. A foreign capture (one in
/// `foreign_captures`), an unrecorded name, a name whose identity no longer
/// matches and an unverifiable name are never unlinked: they stay reported and
/// keep the intent pending.
pub fn replay_pending(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    bounds: DeleteBounds,
) -> Result<ReplayReport> {
    let intents = state::read_intents(state_paths, INTENT_ROWS)?;
    let mut report = ReplayReport {
        examined: intents.total,
        settled: 0,
        rolled_back: 0,
        resumed: 0,
        pending: Vec::new(),
        entries_removed: 0,
    };
    let pending: Vec<IntentRecord> = intents.pending().cloned().collect();
    if pending.is_empty() {
        return Ok(report);
    }
    let registry = registry::read(state_paths)?;
    state_paths.initialize()?;

    for intent in pending {
        let Some(record) = registry.volume(&intent.volume_uuid).cloned() else {
            report.pending.push(PendingRow {
                intent_id: intent.intent_id.clone(),
                unit_name: intent.unit.name.clone(),
                unit_kind: intent.unit.kind.as_str().to_owned(),
                phase: intent.phase,
                reason: format!("volume {} is not registered", intent.volume_uuid),
            });
            continue;
        };
        match replay_one(
            state_paths,
            dirs,
            facts,
            mounts,
            observer,
            &record,
            &intent,
            bounds,
        ) {
            Ok(outcome) => match outcome {
                ReplayOutcome::Settled => report.settled += 1,
                ReplayOutcome::RolledBack => report.rolled_back += 1,
                ReplayOutcome::Resumed(entries) => {
                    report.resumed += 1;
                    report.entries_removed = report.entries_removed.saturating_add(entries);
                }
                ReplayOutcome::Pending(reason) => report.pending.push(PendingRow {
                    intent_id: intent.intent_id.clone(),
                    unit_name: intent.unit.name.clone(),
                    unit_kind: intent.unit.kind.as_str().to_owned(),
                    phase: intent.phase,
                    reason,
                }),
            },
            Err(error) => report.pending.push(PendingRow {
                intent_id: intent.intent_id.clone(),
                unit_name: intent.unit.name.clone(),
                unit_kind: intent.unit.kind.as_str().to_owned(),
                phase: intent.phase,
                reason: error.to_string(),
            }),
        }
    }
    Ok(report)
}

/// What one replay decided.
enum ReplayOutcome {
    /// The both-absent proof settled it.
    Settled,
    /// The unit was still in place, so the intent was rolled back.
    RolledBack,
    /// The deletion resumed and completed.
    Resumed(usize),
    /// It stays pending, with the reason.
    Pending(String),
}

#[allow(clippy::too_many_arguments)]
fn replay_one(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &registry::VolumeRecord,
    intent: &IntentRecord,
    bounds: DeleteBounds,
) -> Result<ReplayOutcome> {
    let opened = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        record,
        &intent.root_dir,
        &intent.folder,
    )?;
    let Some((area_path, area_handle)) = opened else {
        // The area folder is positively gone, so the leaf and the unit are too.
        // Only the durable `removed` phase - recorded after Barge's own final
        // unlink succeeded - proves completion; an absence without it is unknown.
        if intent.phase == IntentPhase::Removed {
            settle(state_paths, intent)?;
            return Ok(ReplayOutcome::Settled);
        }
        return Ok(ReplayOutcome::Pending(
            "the area folder is gone but the removed phase was not recorded, so completion is unknown"
                .to_owned(),
        ));
    };
    let unit = intent.deletion_unit();
    let parent_path = if intent.unit.parent.is_empty() {
        area_path.clone()
    } else {
        area_path.join(&intent.unit.parent)
    };
    let Some(parent) = unit.open_parent(dirs, &area_handle)? else {
        // The unit's parent is positively absent, so the unit and its capture are
        // too - and again only the durable `removed` phase proves completion.
        if intent.phase == IntentPhase::Removed {
            settle(state_paths, intent)?;
            return Ok(ReplayOutcome::Settled);
        }
        return Ok(ReplayOutcome::Pending(
            "the unit's parent is gone but the removed phase was not recorded, so completion is unknown"
                .to_owned(),
        ));
    };

    let mut locks = LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(&intent.volume_uuid)?;
    let _unit_lock = lock_deletion_unit(&mut locks, &unit)?;
    let write = locks.begin_intent_write()?;

    let leaf_facts = dirs.child_facts(&parent, intent.capture_name())?;
    let leaf_matches = leaf_facts.as_ref().is_some_and(|facts| {
        facts.kind == intent.unit.kind.entry_kind()
            && facts.device.get() == intent.unit.device
            && facts.inode == intent.unit.inode
    });
    // One handle to the leaf serves the marker proof and the leaf removal, so the
    // retry that empties it deletes through the descriptor that was verified. A
    // file leaf has no contents to walk, so it has no handle.
    let leaf_handle = if leaf_matches && intent.unit.kind == UnitKind::Directory {
        match dirs.open_child_dir(&parent, intent.capture_name()) {
            Ok(leaf) => Some(leaf),
            Err(error) => {
                return Ok(ReplayOutcome::Pending(format!(
                    "the captured leaf cannot be opened: {error}"
                )))
            }
        }
    } else {
        None
    };
    // The modes this deletion recorded come back first, before any other deletion
    // or reconciliation action - before the marker is even read - so no object is
    // left with a changed mode while a replay decides what to do next. A leaf
    // captured under its own private name is reached from the unit's parent, so its
    // recorded mode is restored even when the visible name is empty.
    if intent.phase != IntentPhase::Deleted {
        match restore_recorded_modes(dirs, &parent, leaf_handle.as_ref(), intent) {
            Ok(None) => {}
            Ok(Some(reason)) => return Ok(ReplayOutcome::Pending(reason)),
            Err(error) => {
                return Ok(ReplayOutcome::Pending(format!(
                    "the recorded modes could not be restored: {error}"
                )))
            }
        }
    }

    let marker_digest = match (&intent.unit.marker, &leaf_handle) {
        (Some(marker), Some(leaf)) => Some(leaf_marker_digest(
            dirs,
            observer,
            intent,
            leaf,
            &marker.filename,
        )?),
        _ => None,
    };

    match intent.phase {
        IntentPhase::Intent => {
            // The rename may not have happened: if the unit is still in place
            // with its recorded identity, the intent is rolled back.
            if unit_still_in_place(dirs, observer, intent, &parent)? {
                state::remove_intent(state_paths, &intent.intent_id)?;
                return Ok(ReplayOutcome::RolledBack);
            }
            if leaf_matches && marker_matches(&marker_digest, intent) {
                if let Some(reason) = recorded_mismatch(&intent.foreign_captures) {
                    return Ok(ReplayOutcome::Pending(reason));
                }
                // The rename happened before its phase was recorded: resume.
                let renamed =
                    state::advance_intent(&write, state_paths, intent, IntentPhase::Renamed)?;
                let report = finish_deletion(
                    &write,
                    state_paths,
                    dirs,
                    facts,
                    observer,
                    &renamed,
                    &parent_path,
                    &parent,
                    bounds,
                );
                return Ok(match report.outcome {
                    DeleteOutcome::Deleted => ReplayOutcome::Resumed(report.entries_removed),
                    _ => ReplayOutcome::Pending(
                        report
                            .reason
                            .unwrap_or_else(|| "the resumed deletion stopped".to_owned()),
                    ),
                });
            }
            Ok(ReplayOutcome::Pending(
                "neither the unit nor the captured leaf matches the recorded identity".to_owned(),
            ))
        }
        IntentPhase::Renamed => {
            if let Some(reason) = recorded_mismatch(&intent.foreign_captures) {
                return Ok(ReplayOutcome::Pending(reason));
            }
            if leaf_matches && marker_matches(&marker_digest, intent) {
                let report = finish_deletion(
                    &write,
                    state_paths,
                    dirs,
                    facts,
                    observer,
                    intent,
                    &parent_path,
                    &parent,
                    bounds,
                );
                return Ok(match report.outcome {
                    DeleteOutcome::Deleted => ReplayOutcome::Resumed(report.entries_removed),
                    _ => ReplayOutcome::Pending(
                        report
                            .reason
                            .unwrap_or_else(|| "the resumed deletion stopped".to_owned()),
                    ),
                });
            }
            Ok(ReplayOutcome::Pending(
                "the captured leaf's recorded identity and marker bytes do not match".to_owned(),
            ))
        }
        IntentPhase::Removed => {
            // Barge's own final unlink was recorded. The leaf must be positively
            // absent for the intent to settle; a leaf that is there again is a new
            // object at the private name, which is unknown.
            if leaf_facts.is_none() {
                state::advance_intent(&write, state_paths, intent, IntentPhase::Deleted)?;
                return Ok(ReplayOutcome::Settled);
            }
            Ok(ReplayOutcome::Pending(
                "the removed phase was recorded but the captured leaf is there again, so completion is unknown"
                    .to_owned(),
            ))
        }
        IntentPhase::MarkerLast => {
            if let Some(reason) = recorded_mismatch(&intent.foreign_captures) {
                return Ok(ReplayOutcome::Pending(reason));
            }
            if leaf_facts.is_none() {
                // The leaf is absent, but the durable completion evidence - Barge's
                // own final unlink - was not recorded: an absence alone is unknown.
                return Ok(ReplayOutcome::Pending(
                    "the captured leaf is gone but the removed phase was not recorded, so completion is unknown"
                        .to_owned(),
                ));
            }
            if !leaf_matches {
                return Ok(ReplayOutcome::Pending(
                    "the leaf and marker do not prove completion".to_owned(),
                ));
            }
            if !marker_absent(&marker_digest, intent) {
                return Ok(ReplayOutcome::Pending(
                    "the unit's marker is still there, so the marker-last phase is not proved"
                        .to_owned(),
                ));
            }
            let journal = CaptureJournal::new(&write, state_paths, intent);
            let leaf_mode = intent
                .captures
                .iter()
                .find(|capture| capture.original == intent.capture_name())
                .and_then(|capture| capture.mode);
            let mut walk = WalkState::new(
                dirs,
                facts,
                observer,
                intent.volume_uuid.clone(),
                DeviceId::new(intent.unit.device),
                intent.mount_hint.clone(),
                bounds,
                &journal,
                Vec::new(),
                intent
                    .unit
                    .marker
                    .as_ref()
                    .map(|marker| marker.filename.clone()),
            )?;
            walk.verify_volume()?;
            let mut counts = Counts::default();
            // The leaf is captured and verified again here too: the visible
            // private name is never removed by name, so a replacement put there
            // cannot be deleted and recorded as a completion.
            match walk.remove_leaf(&parent, intent, leaf_mode, &mut counts)? {
                Removal::Gone => {
                    // The replay's own final unlink is the durable completion
                    // evidence, recorded before the intent settles.
                    let removed =
                        state::advance_intent(&write, state_paths, intent, IntentPhase::Removed)?;
                    state::advance_intent(&write, state_paths, &removed, IntentPhase::Deleted)?;
                    Ok(ReplayOutcome::Settled)
                }
                Removal::Stopped(reason) => Ok(ReplayOutcome::Pending(reason)),
            }
        }
        IntentPhase::Deleted => Ok(ReplayOutcome::Settled),
    }
}

/// Whether the unit is still in place with the recorded identity and marker
/// bytes, which is what makes a rollback safe.
fn unit_still_in_place(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    intent: &IntentRecord,
    parent: &DirHandle,
) -> Result<bool> {
    let Some(facts) = dirs.child_facts(parent, intent.unit_name())? else {
        return Ok(false);
    };
    if facts.kind != intent.unit.kind.entry_kind()
        || facts.device.get() != intent.unit.device
        || facts.inode != intent.unit.inode
    {
        return Ok(false);
    }
    let Some(marker) = &intent.unit.marker else {
        // A markerless unit records no bytes to match; the recorded identity above
        // is what it was rolled back on.
        return Ok(true);
    };
    if intent.unit.kind != UnitKind::Directory {
        return Ok(false);
    }
    let unit_dir = dirs.open_child_dir(parent, intent.unit_name())?;
    Ok(marker_matches(
        &Some(leaf_marker_digest(
            dirs,
            observer,
            intent,
            &unit_dir,
            &marker.filename,
        )?),
        intent,
    ))
}

/// Whether the captured leaf's marker is exactly the bytes the intent recorded.
///
/// A unit that records no marker has no bytes to match, so the leaf's own
/// recorded identity is its whole proof.
fn marker_matches(digest: &Option<MarkerBytes>, intent: &IntentRecord) -> bool {
    match &intent.unit.marker {
        Some(marker) => {
            matches!(digest, Some(MarkerBytes::Digest(bytes)) if bytes == &marker.digest)
        }
        None => true,
    }
}

/// Whether the unit's marker is positively absent, which is what the
/// `marker_last` phase records. A unit that records no marker records no marker
/// step either, so the phase is already true for it.
fn marker_absent(digest: &Option<MarkerBytes>, intent: &IntentRecord) -> bool {
    match &intent.unit.marker {
        Some(_) => matches!(digest, Some(MarkerBytes::Absent)),
        None => true,
    }
}

/// Restore the modes a stopped deletion recorded, before anything else is done.
///
/// The captures are the in-flight chain, outermost first, so each recorded private
/// name is resolved under the one before it: a chain link that is gone ends the
/// walk (nothing deeper can be reached), and a link whose identity no longer
/// matches is unknown, never something to chmod or delete.
fn restore_recorded_modes(
    dirs: &dyn DirOps,
    parent: &DirHandle,
    leaf: Option<&DirHandle>,
    intent: &IntentRecord,
) -> Result<Option<String>> {
    let mut current: Option<DirHandle> = None;
    for record in &intent.captures {
        // A record of the captured leaf itself lives in the unit's parent, whatever
        // private name it is held under now; every other record lives inside the
        // leaf or inside the capture before it.
        let at_leaf = record.original == intent.capture_name();
        let holder: &DirHandle = if at_leaf {
            parent
        } else {
            match &current {
                Some(handle) => handle,
                None => match leaf {
                    Some(leaf) => leaf,
                    None => {
                        return Ok(Some(format!(
                            "`{}` is recorded inside the captured leaf, but the leaf is not at the recorded name, so nothing is restored",
                            record.private
                        )))
                    }
                },
            }
        };
        let Some(facts) = dirs.child_facts(holder, &record.private)? else {
            if at_leaf {
                // The captured leaf itself is gone. Its absence is decided by the
                // phase, not here: the durable `removed` phase proves Barge removed
                // it, and without that phase the replay stays unknown.
                continue;
            }
            // The intent records this capture and the name is gone. Nothing proves
            // Barge unlinked it - the durable `removed` phase is the only such
            // proof - so an absence is unknown and the intent stays pending.
            return Ok(Some(format!(
                "`{}`, the recorded capture of `{}`, is gone without the removed phase, so completion is unknown and its recorded mode is not restored",
                record.private, record.original
            )));
        };
        if !WalkState::matches_capture(record, &facts) {
            return Ok(Some(format!(
                "`{}` is no longer the recorded capture of `{}`, so its recorded mode is not restored and nothing else is done",
                record.private, record.original
            )));
        }
        if facts.kind != EntryKind::Directory {
            // A recorded file capture has no contents and is never chmod'd; there
            // is nothing to restore under it, and no chain to descend. A file leaf
            // is the ordinary case for a file unit, and the phase decides its
            // absence; a directory unit's leaf that is somehow a file is not the
            // recorded object and is reported.
            if at_leaf && intent.unit.kind == UnitKind::Directory {
                return Ok(Some(format!(
                    "`{}` is recorded as the captured leaf but is a {}",
                    record.private,
                    facts.kind.as_str()
                )));
            }
            continue;
        }
        let child = dirs.open_child_dir(holder, &record.private)?;
        // The descriptor that will be chmod'd must be the recorded object: the name
        // can be swapped between the read above and this open.
        let opened = child.identity();
        if opened.device.get() != record.device || opened.inode != record.inode {
            return Ok(Some(format!(
                "`{}` opened as device {} inode {}, not the recorded {} / {}, so its recorded mode is not restored and nothing else is done",
                record.private, opened.device, opened.inode, record.device, record.inode
            )));
        }
        if let Some(mode) = record.mode {
            if let Err(error) = dirs.set_dir_mode(&child, mode & 0o7777) {
                return Ok(Some(format!(
                    "the recorded mode of `{}` could not be restored: {error}",
                    record.private
                )));
            }
        }
        current = Some(child);
    }
    Ok(None)
}

/// The replay stop naming every capture whose verification failed.
///
/// A recorded mismatch proves an object Barge never observed is somewhere in the
/// leaf: under its private name, or back under its original name when the put-back
/// succeeded. No replay resumes such an intent, because resuming would walk that
/// object as an ordinary entry of the unit and unlink it.
fn recorded_mismatch(leftovers: &[ForeignCapture]) -> Option<String> {
    if leftovers.is_empty() {
        return None;
    }
    let named: Vec<String> = leftovers
        .iter()
        .map(|record| {
            format!(
                "`{}` (captured to `{}`, put back {})",
                record.original,
                record.private,
                match record.put_back {
                    PutBackOutcome::Pending => "not recorded",
                    PutBackOutcome::Renamed => "under its own name",
                    PutBackOutcome::Taken => "refused, the name was taken",
                    PutBackOutcome::Failed => "failed",
                }
            )
        })
        .collect();
    Some(format!(
        "the intent records captures that did not verify, so an object Barge never observed is in the leaf and nothing is unlinked: {}",
        named.join(", ")
    ))
}

/// Settle an intent whose area folder or unit parent is positively gone and whose
/// durable `removed` phase was recorded.
fn settle(state_paths: &StatePaths, intent: &IntentRecord) -> Result<()> {
    let unit = intent.deletion_unit();
    let mut locks = LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(&intent.volume_uuid)?;
    let _unit_lock = lock_deletion_unit(&mut locks, &unit)?;
    let write = locks.begin_intent_write()?;
    state::advance_intent(&write, state_paths, intent, IntentPhase::Deleted)?;
    Ok(())
}

/// A lowercase hexadecimal rendering of fresh bytes.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        delete_unit_on_volume, replay_pending, DeleteBounds, DeleteOutcome, ReplayReport,
        CAPTURE_PREFIX,
    };
    use crate::area::{AreaDeclaration, RootKind, ROOT_MARKER_FILE};
    use crate::error::ErrorKind;
    use crate::os::{
        ChildRemoval, DescriptorScan, DescriptorScanRequest, DeviceId, DirOps, EntryKind,
        PlatformStatus, ProcessIdentity, ProcessObserver, RemoveKind, RenameOutcome,
        RetriedUnreadable, RetryEvidence, RetryResolution,
    };
    use crate::state::{self, IntentPhase, IntentRecord, IntentUnit, LockSet, QUARANTINE_PREFIX};
    use crate::testkit::{FakeObserver, Faults, Replacement, VolumeFixture};
    use crate::unit::{DeletionUnit, GrantKind, MarkerFamily, UnitGrant, UnitKind, UnitMarker};

    const PLAN: &str = "abababababababababababababababababababababababababababababababab";

    /// One prepared fixture: an area folder and one deletion unit of any kind.
    struct Prepared {
        fixture: VolumeFixture,
        declaration: AreaDeclaration,
        unit: DeletionUnit,
        area: PathBuf,
    }

    /// A Barge-marked owned root, the shape S5 deleted before S4's units existed.
    fn prepared(label: &str) -> Prepared {
        let fixture = VolumeFixture::new(label);
        fixture.register();
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) =
            fixture.create_root(&fixture.record, &declaration, RootKind::AreaRoot);
        let unit = DeletionUnit::from_owned(&root, &resolved.root_dir, &resolved.folder, PLAN);
        let area = resolved.path.clone();
        Prepared {
            fixture,
            declaration,
            unit,
            area,
        }
    }

    /// A registered fixture volume with the volume's scratch folder as the area
    /// and the recorded grants the planted units need, ready for a unit the test
    /// plants itself.
    ///
    /// S4's claimed and delegated units live inside the volume's own scratch
    /// folder, which is where those grants are scoped, so the fixture resolves
    /// `scratch` directly rather than a declared consumer area.
    fn fixture_area(
        label: &str,
        grants: crate::registry::ScratchGrants,
    ) -> (VolumeFixture, AreaDeclaration, PathBuf) {
        let fixture = VolumeFixture::new(label);
        let mut record = fixture.record.clone();
        record.scratch_grants = grants;
        fixture.register_record(&record);
        let declaration = fixture.declaration("scratch");
        let resolved = fixture.resolved(&record, &declaration);
        let area = resolved.path.clone();
        std::fs::create_dir_all(&area).expect("the scratch folder is created");
        (fixture, declaration, area)
    }

    /// The delegation a foreign-family unit's deletion rests on.
    fn delegated(family: MarkerFamily) -> crate::registry::ScratchGrants {
        crate::registry::ScratchGrants {
            delegations: vec![crate::registry::ScratchDelegation {
                family: match family {
                    MarkerFamily::EffigyScratch => crate::registry::MarkerFamily::EffigyScratch,
                    MarkerFamily::OxideScratch => crate::registry::MarkerFamily::OxideScratch,
                    MarkerFamily::Barge => {
                        panic!("a Barge root needs no delegation")
                    }
                },
                by: "the family's owner".to_owned(),
                declared_ms: 1,
            }],
            claims: Vec::new(),
        }
    }

    /// The claim an unmarked unit's deletion rests on.
    fn claimed(
        scope: crate::registry::ClaimScope,
        unit: crate::registry::ClaimUnit,
    ) -> crate::registry::ScratchGrants {
        crate::registry::ScratchGrants {
            delegations: Vec::new(),
            claims: vec![crate::registry::ScratchClaim {
                scope,
                unit,
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

    /// The identity the plan records for one planted unit, read from the
    /// filesystem exactly as a plan would observe it.
    #[allow(clippy::too_many_arguments)]
    fn plan_unit(
        fixture: &VolumeFixture,
        declaration: &AreaDeclaration,
        parent: &str,
        area: &Path,
        name: &str,
        kind: UnitKind,
        marker: Option<UnitMarker>,
        grant: UnitGrant,
    ) -> DeletionUnit {
        let path = if parent.is_empty() {
            area.join(name)
        } else {
            area.join(parent).join(name)
        };
        let facts = crate::os::platform()
            .path_facts(&path)
            .expect("the planted unit is readable")
            .expect("the planted unit exists");
        DeletionUnit {
            volume_uuid: fixture.uuid.clone(),
            root_dir: fixture.record.root_dir.clone(),
            folder: declaration.folder.clone(),
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

    /// A directory unit carrying one foreign family's marker file, as a delegated
    /// Effigy or Oxide run is.
    fn prepared_foreign(label: &str, family: MarkerFamily) -> Prepared {
        let (fixture, declaration, area) = fixture_area(label, delegated(family));
        let name = "run.foreign";
        let marker_bytes = b"{\"run\": \"foreign\"}";
        std::fs::create_dir(area.join(name)).expect("the foreign unit is created");
        std::fs::write(area.join(name).join(family.marker_file()), marker_bytes)
            .expect("the foreign marker is written");
        let marker = UnitMarker::new(family, crate::digest::sha256_hex(marker_bytes));
        let unit = plan_unit(
            &fixture,
            &declaration,
            "",
            &area,
            name,
            UnitKind::Directory,
            Some(marker),
            UnitGrant::delegation(family),
        );
        Prepared {
            fixture,
            declaration,
            unit,
            area,
        }
    }

    /// An unmarked claimed directory unit: one child of the `alpha` project folder
    /// inside the volume's scratch folder.
    fn prepared_unmarked(label: &str) -> Prepared {
        let (fixture, declaration, area) = fixture_area(
            label,
            claimed(
                crate::registry::ClaimScope::Project {
                    name: "alpha".to_owned(),
                },
                crate::registry::ClaimUnit::Child,
            ),
        );
        std::fs::create_dir_all(area.join("alpha")).expect("the project folder exists");
        let name = "unmarked-run";
        std::fs::create_dir(area.join("alpha").join(name)).expect("the unmarked unit is created");
        let unit = plan_unit(
            &fixture,
            &declaration,
            "alpha",
            &area,
            name,
            UnitKind::Directory,
            None,
            UnitGrant::claim("project:alpha"),
        );
        Prepared {
            fixture,
            declaration,
            unit,
            area,
        }
    }

    /// A claimed top-level file unit.
    fn prepared_file(label: &str) -> Prepared {
        let (fixture, declaration, area) = fixture_area(
            label,
            claimed(
                crate::registry::ClaimScope::TopLevelAdhoc,
                crate::registry::ClaimUnit::Entry,
            ),
        );
        let name = "scratch-report.txt";
        std::fs::write(area.join(name), b"bytes").expect("the file unit is written");
        let unit = plan_unit(
            &fixture,
            &declaration,
            "",
            &area,
            name,
            UnitKind::File,
            None,
            UnitGrant::claim("top_level_adhoc"),
        );
        Prepared {
            fixture,
            declaration,
            unit,
            area,
        }
    }

    impl Prepared {
        fn bounds(&self) -> DeleteBounds {
            DeleteBounds {
                retry_pause: Duration::ZERO,
                ..DeleteBounds::default()
            }
        }

        fn delete(&self, dirs: &Faults, facts: &crate::testkit::FakeVolume) -> super::DeleteReport {
            self.delete_with(dirs, facts, &self.fixture.observer())
        }

        fn delete_with(
            &self,
            dirs: &Faults,
            facts: &crate::testkit::FakeVolume,
            observer: &dyn ProcessObserver,
        ) -> super::DeleteReport {
            let mounts = self.fixture.mounts();
            delete_unit_on_volume(
                &self.fixture.state,
                dirs,
                facts,
                &mounts,
                observer,
                &self.declaration,
                None,
                &self.unit,
                self.bounds(),
            )
            .expect("the deletion reports rather than fails")
        }

        /// The unit's absolute path on the fixture volume.
        fn unit_path(&self) -> PathBuf {
            self.unit.path_in(&self.area)
        }

        /// The directory the unit lives in, which is where its capture name lives.
        fn parent_path(&self) -> PathBuf {
            if self.unit.parent.is_empty() {
                self.area.clone()
            } else {
                self.area.join(&self.unit.parent)
            }
        }

        /// Plant one payload file inside the unit.
        fn payload(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.unit_path().join(name);
            VolumeFixture::payload(&self.unit_path(), name, bytes);
            path
        }

        fn replay(&self) -> ReplayReport {
            let faults = Faults::new();
            let facts = self.fixture.facts();
            let mounts = self.fixture.mounts();
            let observer = self.fixture.observer();
            replay_pending(
                &self.fixture.state,
                &faults,
                &facts,
                &mounts,
                &observer,
                self.bounds(),
            )
            .expect("the replay reports")
        }

        fn quarantine(&self) -> PathBuf {
            self.parent_path()
                .join(format!("{QUARANTINE_PREFIX}{:032x}", 1))
        }

        fn pending_intents(&self) -> Vec<IntentRecord> {
            state::read_intents(&self.fixture.state, 64)
                .expect("the journal reads")
                .pending()
                .cloned()
                .collect()
        }
    }

    /// The fixture observer whose first custody scan completes and whose second
    /// answers unreadable with a bounded retry record. It is the deletion's
    /// re-decision after the intent is published: that refusal must keep the
    /// evidence, not reduce it to a reason string.
    struct RefusesTheSecondScan {
        inner: FakeObserver,
        scans: std::cell::Cell<usize>,
    }

    impl ProcessObserver for RefusesTheSecondScan {
        fn effective_uid(&self) -> crate::error::Result<u32> {
            self.inner.effective_uid()
        }

        fn boot_identity(&self) -> crate::error::Result<String> {
            self.inner.boot_identity()
        }

        fn process_start(&self, pid: u32) -> crate::error::Result<Option<u64>> {
            self.inner.process_start(pid)
        }

        fn verify_platform_binary(
            &self,
            classified: ProcessIdentity,
        ) -> crate::error::Result<PlatformStatus> {
            self.inner.verify_platform_binary(classified)
        }

        fn scan_open_descriptors(
            &self,
            request: &DescriptorScanRequest<'_>,
        ) -> crate::error::Result<DescriptorScan> {
            let seen = self.scans.get().saturating_add(1);
            self.scans.set(seen);
            if seen == 1 {
                return self.inner.scan_open_descriptors(request);
            }
            Ok(DescriptorScan::Unreadable {
                reason: "the open descriptors of pid 4242 (`dmd`, start 7) are still unreadable: Operation not permitted (os error 1) on 2 attempts (retries: 1)".to_owned(),
                retry: RetryEvidence {
                    retried_unreadable: 1,
                    retried: vec![RetriedUnreadable {
                        pid: 4_242,
                        command: Some("dmd".to_owned()),
                        start: 7,
                        errnos: vec![1, 1],
                        identity_changes: 0,
                        resolution: RetryResolution::Unreadable,
                    }],
                },
            })
        }
    }

    #[test]
    fn a_custody_refusal_after_the_intent_keeps_its_bounded_retry_record() {
        let prepared = prepared("delete-custody-after-intent");
        let dirs = Faults::new();
        let facts = prepared.fixture.facts();
        let observer = RefusesTheSecondScan {
            inner: prepared.fixture.observer(),
            scans: std::cell::Cell::new(0),
        };
        let report = prepared.delete_with(&dirs, &facts, &observer);
        assert_eq!(report.outcome, DeleteOutcome::Pending);
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("after the intent"),
            "the refusal names when it happened: {:?}",
            report.reason
        );
        let retry = report
            .custody_retry
            .expect("the refusal keeps its bounded retry record");
        assert_eq!(retry.retried_unreadable, 1);
        let record = retry.retried.first().expect("the retried pid");
        assert_eq!(record.pid, 4_242);
        assert_eq!(record.command.as_deref(), Some("dmd"));
        assert_eq!(record.errnos, vec![1, 1]);
        assert_eq!(record.resolution, RetryResolution::Unreadable);
    }

    /// Publish one intent for the prepared root, advancing it through `phases`.
    fn publish(prepared: &Prepared, phases: &[IntentPhase]) -> IntentRecord {
        let intent_id = format!("{:032x}", 1);
        let now = state::now_ms();
        let mut intent = IntentRecord {
            schema: crate::contract::INTENT_SCHEMA.to_owned(),
            intent_id: intent_id.clone(),
            volume_uuid: prepared.unit.volume_uuid.clone(),
            mount_hint: prepared.fixture.root.clone(),
            root_dir: prepared.unit.root_dir.clone(),
            folder: prepared.unit.folder.clone(),
            unit: IntentUnit {
                parent: prepared.unit.parent.clone(),
                name: prepared.unit.name.clone(),
                kind: prepared.unit.kind,
                device: prepared.unit.device.get(),
                inode: prepared.unit.inode,
                marker: prepared.unit.marker.clone(),
                capture_name: format!("{QUARANTINE_PREFIX}{intent_id}"),
                grant: prepared.unit.grant.clone(),
            },
            plan_digest: PLAN.to_owned(),
            phase: IntentPhase::Intent,
            generation: 1,
            recorded_ms: now,
            phase_ms: now,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
        };
        let mut locks = LockSet::new(prepared.fixture.state.clone());
        let _volume = locks
            .lock_volume(&prepared.fixture.uuid)
            .expect("the volume lock");
        let _root = super::lock_deletion_unit(&mut locks, &prepared.unit).expect("the unit lock");
        let write = locks.begin_intent_write().expect("the intent step");
        state::publish_intent(&write, &prepared.fixture.state, &intent)
            .expect("the intent publishes");
        for phase in phases {
            intent = state::advance_intent(&write, &prepared.fixture.state, &intent, *phase)
                .expect("the phase advances");
        }
        intent
    }

    /// Replay pending intents through explicit seams, for a test that needs its own
    /// dirs or facts.
    fn replay_with(
        state_paths: &crate::state::StatePaths,
        dirs: &Faults,
        facts: &crate::testkit::FakeVolume,
        mounts: &crate::testkit::FakeMounts,
        observer: &dyn ProcessObserver,
        bounds: DeleteBounds,
    ) -> crate::error::Result<ReplayReport> {
        replay_pending(state_paths, dirs, facts, mounts, observer, bounds)
    }

    /// Record one capture on the prepared intent, after its phases are published.
    fn publish_capture(prepared: &Prepared, record: &state::CaptureRecord) {
        let mut locks = LockSet::new(prepared.fixture.state.clone());
        let _volume = locks
            .lock_volume(&prepared.fixture.uuid)
            .expect("the volume lock");
        let _root = super::lock_deletion_unit(&mut locks, &prepared.unit).expect("the unit lock");
        let write = locks.begin_intent_write().expect("the intent step");
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        let intent = intents.records.first().expect("the intent");
        state::record_captures(
            &write,
            &prepared.fixture.state,
            intent,
            std::slice::from_ref(record),
        )
        .expect("the capture is recorded");
    }

    fn rename_to_quarantine(prepared: &Prepared) {
        std::fs::rename(prepared.unit_path(), prepared.quarantine())
            .expect("the unit is quarantined");
    }

    #[test]
    fn an_owned_root_is_deleted_whole_and_the_marker_is_unlinked_last() {
        let prepared = prepared("delete-clean");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        VolumeFixture::payload(&root, "sub/nested/b.bin", b"more");
        VolumeFixture::payload(&root, ".DS_Store", b"finder");
        VolumeFixture::payload(&root, "._payload.txt", b"finder");

        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert_eq!(report.phase, IntentPhase::Deleted);
        assert_eq!(report.outcome, DeleteOutcome::Deleted);
        assert!(report.marker_removed, "the marker was unlinked last");
        assert!(!root.exists(), "the whole root is gone");
        assert!(!report.quarantine.exists(), "the quarantine leaf is gone");
        assert!(report.entries_removed >= 4, "{report:?}");
        assert!(report.custody.is_some(), "the evidence is reported");

        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.total, 1, "the settled intent stays as a record");
        assert!(intents.pending().next().is_none(), "it is settled");
    }

    #[test]
    fn an_unknown_entry_stops_the_delete_and_keeps_the_root() {
        let prepared = prepared("delete-unknown-entry");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        VolumeFixture::payload(&root, "odd.txt", b"an entry on another device");

        // The entry reports another device, which is what an unexpected entry
        // looks like to the walk: it is neither a plain file of this volume nor
        // Finder metadata this delete tolerates.
        let faults = Faults::new().with_device_shift(
            "odd.txt",
            DeviceId::new(prepared.fixture.device.get().saturating_add(9)),
        );
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::Renamed, "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("odd.txt"),
            "{:?}",
            report.reason
        );
        assert!(
            report.quarantine.join("payload.txt").is_file(),
            "the root is kept"
        );
        assert!(report.quarantine.join(ROOT_MARKER_FILE).is_file());
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].phase, IntentPhase::Renamed);
    }

    #[test]
    fn a_symlink_inside_the_quarantine_stops_the_delete() {
        let prepared = prepared("delete-symlink");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        std::os::unix::fs::symlink("/somewhere/else", root.join("link")).expect("the link exists");

        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{:?}", report.reason);
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("symbolic link"),
            "{:?}",
            report.reason
        );
        let leaf = report.quarantine.clone();
        assert!(std::fs::symlink_metadata(leaf.join("link"))
            .expect("the link is still there")
            .file_type()
            .is_symlink());
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the marker is kept");
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
    }

    #[test]
    fn enotempty_three_times_leaves_a_pending_intent_and_the_quarantine_leaf() {
        let prepared = prepared("delete-enotempty");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");

        let faults = Faults::new();
        // The leaf itself is removed under its private capture name, so that is
        // where the retries are refused.
        faults.failing_remove_prefix(CAPTURE_PREFIX, 3);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::MarkerLast, "{report:?}");
        assert!(report.marker_removed, "the marker was unlinked last");
        assert!(
            !capture_names(report.quarantine.parent().expect("the area folder")).is_empty(),
            "the captured leaf is kept"
        );
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("not empty"),
            "{:?}",
            report.reason
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].phase, IntentPhase::MarkerLast);
    }

    #[test]
    fn an_injected_device_change_stops_before_the_first_unlink() {
        let prepared = prepared("delete-device-change");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");

        let facts = prepared.fixture.facts_shared();
        let faults = Faults::new().shifting_volume(facts.clone());
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{:?}", report.reason);
        assert_eq!(report.phase, IntentPhase::Renamed);
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("device"),
            "{:?}",
            report.reason
        );
        assert!(report.quarantine.join("payload.txt").is_file());
        assert!(report.quarantine.join(ROOT_MARKER_FILE).is_file());
    }

    #[test]
    fn replay_rolls_back_an_intent_whose_root_is_still_in_place() {
        let prepared = prepared("replay-rollback");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);

        let report = prepared.replay();
        assert_eq!(report.rolled_back, 1);
        assert_eq!(report.resumed, 0);
        assert!(report.is_settled());
        assert!(root.join("payload.txt").is_file(), "the root is untouched");
        assert!(
            prepared.pending_intents().is_empty(),
            "the rolled-back intent is removed"
        );
    }

    #[test]
    fn replay_resumes_a_rename_whose_phase_was_not_recorded() {
        let prepared = prepared("replay-rename-unrecorded");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);

        let report = prepared.replay();
        assert_eq!(report.resumed, 1, "{report:?}");
        assert!(report.is_settled());
        assert!(!root.exists());
        assert!(!prepared.quarantine().exists());
        let intents = prepared.pending_intents();
        assert!(intents.is_empty());
    }

    #[test]
    fn replay_resumes_a_renamed_intent() {
        let prepared = prepared("replay-renamed");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        publish(&prepared, &[IntentPhase::Renamed]);

        // Publishing a second intent would be a second record; the helper uses
        // one id, so the advance above replaced the same record.
        let report = prepared.replay();
        assert_eq!(report.resumed, 1, "{report:?}");
        assert!(report.is_settled());
        assert!(!prepared.quarantine().exists());
    }

    #[test]
    fn replay_removes_a_marker_last_leaf_and_settles() {
        let prepared = prepared("replay-marker-last-leaf");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        std::fs::remove_file(leaf.join(ROOT_MARKER_FILE)).expect("the marker is removed");
        std::fs::remove_file(leaf.join("payload.txt")).expect("the payload is removed");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);

        let report = prepared.replay();
        assert_eq!(report.settled, 1, "{report:?}");
        assert!(report.is_settled());
        assert!(!leaf.exists(), "the leaf is removed");
    }

    #[test]
    fn replay_keeps_an_absent_leaf_without_removed_pending() {
        // An absence is never proof: without the durable `removed` phase - written
        // only after Barge's own final unlink succeeded - a gone leaf is unknown.
        let prepared = prepared("replay-both-absent");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        std::fs::remove_dir_all(prepared.quarantine()).expect("the leaf is removed");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);

        let report = prepared.replay();
        assert_eq!(report.settled, 0, "{report:?}");
        assert_eq!(report.pending.len(), 1, "{report:?}");
        assert!(
            report.pending[0].reason.contains("unknown"),
            "{:?}",
            report.pending[0].reason
        );
        assert_eq!(prepared.pending_intents().len(), 1);
    }

    #[test]
    fn replay_settles_an_absent_leaf_with_the_removed_phase() {
        // The completion evidence: Barge's own final unlink was recorded, and the
        // leaf is positively absent, so the intent settles.
        let prepared = prepared("replay-removed-absent");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        std::fs::remove_dir_all(prepared.quarantine()).expect("the leaf is removed");
        publish(
            &prepared,
            &[
                IntentPhase::Renamed,
                IntentPhase::MarkerLast,
                IntentPhase::Removed,
            ],
        );

        let report = prepared.replay();
        assert_eq!(report.settled, 1, "{report:?}");
        assert!(report.is_settled());
        assert_eq!(prepared.pending_intents().len(), 0);
    }

    #[test]
    fn replay_never_infers_completion_from_a_missing_marker() {
        let prepared = prepared("replay-missing-marker");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        std::fs::remove_file(leaf.join(ROOT_MARKER_FILE)).expect("the marker is removed");
        publish(&prepared, &[IntentPhase::Renamed]);

        let report = prepared.replay();
        assert_eq!(report.settled, 0);
        assert_eq!(report.resumed, 0);
        assert_eq!(report.pending.len(), 1, "{report:?}");
        assert!(report.pending[0].reason.contains("do not match"));
        assert!(leaf.is_dir(), "the leaf stays pending");
    }

    #[test]
    fn a_finder_shaped_directory_that_appears_after_classification_stops_the_delete() {
        // Finder tolerance is `.DS_Store` or `._name` as a regular file of this
        // user on this device. A directory that happens to carry that name is
        // not Finder metadata: it stops the delete and never widens it.
        let prepared = prepared("delete-finder-directory");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");

        let faults = Faults::new();
        faults.planting_directory(root.parent().expect("the area folder"), ".DS_Store", 1);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::MarkerLast, "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("appeared after the classification"),
            "{report:?}"
        );
        let area = report
            .quarantine
            .parent()
            .expect("the area folder")
            .to_path_buf();
        let planted = find_entry(&area, ".DS_Store").expect("the planted entry is still there");
        assert!(
            std::fs::symlink_metadata(&planted)
                .expect("the planted entry is still there")
                .file_type()
                .is_dir(),
            "the Finder-shaped directory is never recursively deleted"
        );
        assert!(
            !capture_names(&area).is_empty(),
            "the captured leaf is kept"
        );
    }

    #[test]
    fn a_settled_intent_is_not_pending_work() {
        let prepared = prepared("replay-settled");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted());

        let replay = prepared.replay();
        assert_eq!(replay.examined, 1);
        assert_eq!(replay.settled, 0);
        assert_eq!(replay.pending.len(), 0);
        assert!(replay.is_settled());
    }

    #[test]
    fn a_plan_digest_that_is_not_sha256_refuses_before_any_effect() {
        let prepared = prepared("delete-plan-digest");
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let mut unit = prepared.unit.clone();
        unit.plan_digest = "not-a-digest".to_owned();
        let error = delete_unit_on_volume(
            &prepared.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &prepared.declaration,
            None,
            &unit,
            prepared.bounds(),
        )
        .expect_err("a bad plan digest refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(prepared.unit_path().is_dir(), "nothing happened");
        assert!(
            prepared
                .fixture
                .state
                .intents_dir()
                .read_dir()
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "no intent is written"
        );
    }

    #[test]
    fn a_root_that_is_not_there_refuses_before_any_effect() {
        let prepared = prepared("delete-absent-root");
        std::fs::remove_dir_all(prepared.unit_path()).expect("the root is removed");
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let error = delete_unit_on_volume(
            &prepared.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &prepared.declaration,
            None,
            &prepared.unit,
            prepared.bounds(),
        )
        .expect_err("a missing root refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_live_hold_refuses_before_the_intent() {
        let prepared = prepared("delete-live-hold");
        let path = prepared
            .fixture
            .state
            .holds_dir()
            .join(&prepared.unit.name)
            .join("writer.json");
        std::fs::create_dir_all(path.parent().expect("the hold parent")).expect("the holds dir");
        std::fs::write(
            &path,
            VolumeFixture::hold_record(
                &prepared.unit.name,
                "writer",
                std::process::id(),
                7_000_000,
                crate::testkit::FIXTURE_BOOT,
            ),
        )
        .expect("the hold is written");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");

        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let error = delete_unit_on_volume(
            &prepared.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &prepared.declaration,
            None,
            &prepared.unit,
            prepared.bounds(),
        )
        .expect_err("a live holder refuses");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
        assert!(prepared.unit_path().is_dir(), "the root is untouched");
        assert!(
            prepared.pending_intents().is_empty(),
            "no intent is written"
        );
    }

    #[test]
    fn a_finder_file_that_reappears_during_the_delete_is_tolerated() {
        let prepared = prepared("delete-finder-reappears");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        // A `.DS_Store` is planted after classification by the deletion's own
        // fault injection: the directory reports ENOTEMPTY once, and the retry
        // finds and removes the Finder file rather than stopping.
        let faults = Faults::new();
        faults.failing_remove_prefix(QUARANTINE_PREFIX, 1);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert!(!root.exists());
    }

    #[test]
    fn the_report_names_the_quarantine_leaf_while_an_intent_is_pending() {
        let prepared = prepared("delete-pending-leaf");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "odd.txt", b"bytes");
        let faults = Faults::new().with_device_shift(
            "odd.txt",
            DeviceId::new(prepared.fixture.device.get().saturating_add(3)),
        );
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let expected: PathBuf = root
            .parent()
            .expect("the area folder")
            .join(format!("{QUARANTINE_PREFIX}{}", report.intent_id));
        assert_eq!(report.quarantine, expected);
        assert!(Path::new(&report.quarantine).is_dir());
    }

    /// The entries a racer test replaces, at three depths.
    const RACED_ENTRIES: [&str; 4] = [
        "payload.txt",
        "second.txt",
        "sub/nested.txt",
        "sub/deep/item.bin",
    ];

    /// Find one name anywhere under `directory`.
    ///
    /// A raced entry whose parent the deletion captured is put back inside that
    /// parent, which is itself held under a private name, so a test cannot assume
    /// the entry stayed at the path it was planted at.
    fn find_entry(directory: &Path, name: &str) -> Option<PathBuf> {
        let mut nested = Vec::new();
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            if entry.file_name().to_string_lossy() == name {
                return Some(entry.path());
            }
            if entry.path().is_dir() {
                nested.push(entry.path());
            }
        }
        nested
            .iter()
            .find_map(|directory| find_entry(directory, name))
    }

    /// Find a private name whose object holds `bytes`, anywhere under `directory`.
    fn find_private_with(directory: &Path, bytes: &[u8]) -> Option<PathBuf> {
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(CAPTURE_PREFIX)
                && std::fs::read(&path).ok().as_deref() == Some(bytes)
            {
                return Some(path);
            }
            if path.is_dir() {
                if let Some(found) = find_private_with(&path, bytes) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// The file name of one raced entry.
    fn raced_name(raced: &str) -> &str {
        raced.rsplit('/').next().unwrap_or(raced)
    }

    /// Plant the racer fixture's tree under `root`.
    fn plant_tree(root: &Path, bytes: &[u8]) {
        for name in RACED_ENTRIES {
            VolumeFixture::payload(root, name, bytes);
        }
    }

    /// The check-then-unlink-by-name walk this change replaces: it reads each
    /// entry's facts and then removes it by name, which is exactly the race. It
    /// exists only as the counterfactual a test runs.
    fn check_then_unlink(
        dirs: &Faults,
        handle: &crate::os::DirHandle,
        removed: &mut usize,
    ) -> crate::error::Result<()> {
        for entry in dirs.list_children(handle, 64)?.entries {
            if entry.name == ROOT_MARKER_FILE {
                continue;
            }
            let Some(facts) = dirs.child_facts(handle, &entry.name)? else {
                continue;
            };
            if facts.kind == EntryKind::Directory {
                let child = dirs.open_child_dir(handle, &entry.name)?;
                check_then_unlink(dirs, &child, removed)?;
            } else if dirs.remove_child(handle, &entry.name, RemoveKind::File)?
                == ChildRemoval::Removed
            {
                *removed = removed.saturating_add(1);
            }
        }
        Ok(())
    }

    /// The private capture names one directory holds: what a stopped deletion
    /// left behind, and what a put-back collision keeps beside the original name.
    fn capture_names(directory: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(CAPTURE_PREFIX)
            })
            .map(|entry| entry.path())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_racer_that_replaces_each_entry_after_its_check_gets_nothing_removed() {
        // The racer replaces an entry between the deletion's check of it and the
        // capture that follows: the window a check-then-unlink by name loses. The
        // captured object no longer matches the observation, so nothing is
        // unlinked and the racer's object survives under its own name.
        let prepared = prepared("delete-racer-file");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let faults = Faults::new();
        let foreign = b"the racer's object";
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::Renamed, "{report:?}");
        assert!(!report.marker_removed, "the marker is kept");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("put back"),
            "the stop reports the put-back: {:?}",
            report.reason
        );
        let leaf = report.quarantine.clone();
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the racer's object is kept"),
            foreign,
            "the object that was not the observed one is never unlinked"
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the root is kept");
        assert!(capture_names(&leaf).is_empty(), "nothing is left private");
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1, "the intent stays pending");
        assert_eq!(pending[0].phase, IntentPhase::Renamed);
    }

    #[test]
    fn a_racer_that_replaces_a_directory_is_captured_and_never_recursed_into() {
        let prepared = prepared("delete-racer-directory");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        std::fs::create_dir(root.join("sub")).expect("the directory exists");
        let faults = Faults::new();
        faults.racing(
            &area,
            "sub",
            2,
            Replacement::Directory {
                name: "foreign.txt".to_owned(),
                bytes: b"the racer's object".to_vec(),
            },
        );
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
        let leaf = report.quarantine.clone();
        assert_eq!(
            std::fs::read(leaf.join("sub/foreign.txt"))
                .expect("the racer's directory is kept whole"),
            b"the racer's object",
            "an unverified directory is never recursed into"
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file());
    }

    #[test]
    fn a_racer_that_replaces_a_file_with_a_symlink_is_never_followed() {
        let prepared = prepared("delete-racer-symlink");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "link-swapped.txt", b"mine");
        let victim = area.join("victim.txt");
        std::fs::write(&victim, b"never removed").expect("the victim exists");
        let faults = Faults::new();
        faults.racing(
            &area,
            "link-swapped.txt",
            2,
            Replacement::Symlink(victim.clone()),
        );
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
        assert_eq!(
            std::fs::read(&victim).expect("the link target is untouched"),
            b"never removed"
        );
        let leaf = report.quarantine.clone();
        assert!(
            std::fs::symlink_metadata(leaf.join("link-swapped.txt"))
                .expect("the swapped link is put back")
                .file_type()
                .is_symlink(),
            "a captured symlink is put back as itself, never followed"
        );
    }

    #[test]
    fn a_put_back_collision_leaves_the_object_under_its_private_name() {
        let prepared = prepared("delete-put-back-collision");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let faults = Faults::new();
        let foreign = b"the racer's object";
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        faults.occupying_after_capture("payload.txt");
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
        let leaf = report.quarantine.clone();
        let reason = report.reason.unwrap_or_default();
        assert!(reason.contains("payload.txt"), "{reason}");
        assert!(
            reason.contains(CAPTURE_PREFIX),
            "both names are reported: {reason}"
        );
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the name that was taken back"),
            b"a racer took this name back"
        );
        let private = capture_names(&leaf);
        assert_eq!(
            private.len(),
            1,
            "the captured object keeps its private name"
        );
        assert_eq!(
            std::fs::read(&private[0]).expect("the captured object is kept"),
            foreign
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file());

        // The unverified capture is durable evidence: the private name, the name
        // it came from, the observation it failed to match and why.
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        let recorded = &pending[0].foreign_captures;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].original, "payload.txt");
        assert_eq!(
            recorded[0].private,
            private[0]
                .file_name()
                .expect("a private name")
                .to_str()
                .expect("an ASCII name")
        );
        assert_eq!(recorded[0].kind, "file");
        let captured = recorded[0]
            .captured
            .as_ref()
            .expect("the record keeps what the captured object was");
        assert_eq!(captured.kind, "file");
        assert_ne!(
            captured.inode, recorded[0].inode,
            "the record keeps the identity that did not match"
        );
        assert_eq!(recorded[0].put_back, state::PutBackOutcome::Taken);
        assert!(
            !recorded[0].reason.is_empty(),
            "the record says why the capture did not verify"
        );
    }

    #[test]
    fn finder_metadata_that_reappears_mid_delete_is_removed_by_class() {
        let prepared = prepared("delete-finder-recreated");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        // Finder recreates its store after the classification and while the leaf
        // is being emptied: it has no recorded inode, so it is verified by class.
        let faults = Faults::new();
        faults.planting_file(&area, ".DS_Store", 1);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert!(!root.exists());
        assert!(!report.quarantine.exists());
    }

    #[test]
    fn an_exclusive_rename_onto_an_existing_name_refuses() {
        let fixture = VolumeFixture::new("exclusive-rename");
        let dirs = Faults::new();
        let handle = dirs.open_path(&fixture.root).expect("the directory opens");
        std::fs::write(fixture.root.join("a.txt"), b"source").expect("the source exists");
        std::fs::write(fixture.root.join("b.txt"), b"target").expect("the target exists");

        let outcome = dirs
            .rename_child(&handle, "a.txt", "b.txt")
            .expect("the refusal is an answer, not a failure");
        assert_eq!(outcome, RenameOutcome::TargetExists);
        assert_eq!(
            std::fs::read(fixture.root.join("a.txt")).expect("the source is untouched"),
            b"source"
        );
        assert_eq!(
            std::fs::read(fixture.root.join("b.txt")).expect("the target is untouched"),
            b"target",
            "an existing name is never replaced"
        );

        std::fs::create_dir(fixture.root.join("c.txt")).expect("the directory exists");
        assert_eq!(
            dirs.rename_child(&handle, "a.txt", "c.txt")
                .expect("the refusal is an answer"),
            RenameOutcome::TargetExists,
            "a directory target is refused the same way"
        );
        assert_eq!(
            dirs.rename_child(&handle, "a.txt", "free.txt")
                .expect("a free name is renamed onto"),
            RenameOutcome::Renamed
        );
        assert!(fixture.root.join("free.txt").is_file());
    }

    #[test]
    fn replay_never_unlinks_a_leftover_private_name_it_cannot_verify() {
        // A private name is not an ordinary entry: nothing recorded the identity
        // of the object under it for a replay to check, so the replay keeps it,
        // reports it and leaves the intent pending.
        let prepared = prepared("replay-leftover-private");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        std::fs::remove_file(leaf.join(ROOT_MARKER_FILE)).expect("the marker is removed");
        let private = leaf.join(format!("{CAPTURE_PREFIX}{}", "ab".repeat(16)));
        std::fs::rename(leaf.join("payload.txt"), &private).expect("a capture is left behind");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);

        let report = prepared.replay();
        assert_eq!(report.settled, 0, "{report:?}");
        assert_eq!(report.pending.len(), 1, "{report:?}");
        assert!(
            report.pending[0].reason.contains("unverifiable"),
            "{:?}",
            report.pending[0].reason
        );
        // The replay captured the leaf itself before walking it, so the leftover is
        // now inside the leaf's own private name; it is never unlinked.
        let area = leaf.parent().expect("the area folder").to_path_buf();
        let kept = find_entry(
            &area,
            private
                .file_name()
                .expect("a name")
                .to_str()
                .expect("a name"),
        )
        .expect("the leftover is never unlinked");
        assert!(kept.is_file(), "the leftover is still a file");
        assert_eq!(prepared.pending_intents().len(), 1);
    }

    /// The mode a path has, as the tests read it.
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("the path has metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_private_name_that_vanishes_mid_run_leaves_the_intent_pending() {
        // Rule 1: another process moving the captured object between Barge's check
        // and its unlink is never a completion. The removal answers "already gone",
        // the intent stays pending and both names are reported.
        let prepared = prepared("delete-capture-vanishes");
        let root = prepared.unit_path().clone();
        std::fs::create_dir(root.join("sub")).expect("the directory exists");
        let faults = Faults::new();
        faults.vanishing_capture();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("another process may have moved"),
            "{:?}",
            report.reason
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].phase.is_pending());
        assert!(
            capture_names(&report.quarantine).is_empty(),
            "the object was moved away, so no private name holds it"
        );
        // The vanished object is gone and the leaf holds nothing of it, so the
        // intent is the only record that it was captured and never unlinked by
        // Barge.
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].captures[0].original, "sub");
        assert!(intents.records[0].captures[0].kind == "directory");

        // A replay may not treat the gone capture as a completed step: the durable
        // `removed` phase is the only proof, so it stays pending.
        let replay = prepared.replay();
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0]
                .reason
                .contains("gone without the removed phase"),
            "{:?}",
            replay.pending[0].reason
        );
    }

    #[test]
    fn classification_never_recurses_into_a_swapped_directory() {
        // The classification walks only the directory it observed: a swap between
        // the read and the open stops it, so no unverified directory is classified
        // or recorded as expected.
        let prepared = prepared("classify-swapped-directory");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "sub/inside.txt", b"bytes");
        let faults = Faults::new();
        faults.swapping_dir_on_open("sub");
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("so it is not classified"),
            "{:?}",
            report.reason
        );
        let leaf = report.quarantine.clone();
        assert!(
            find_entry(&leaf, "sub").is_some(),
            "the racer's directory is kept"
        );
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert!(intents.records[0].phase.is_pending());
        assert!(
            intents.records[0].captures.is_empty(),
            "nothing was captured or recorded from the swapped directory"
        );
    }

    #[test]
    fn replay_restores_a_captured_leaf_mode_when_the_visible_name_is_empty() {
        // The leaf can be captured under its own private name, so the visible
        // quarantine name is empty. Its recorded mode is still restored first,
        // reached from the area folder.
        let prepared = prepared("replay-captured-leaf-mode");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        let private = format!("{CAPTURE_PREFIX}{}", "ad".repeat(16));
        let captured = area.join(&private);
        std::fs::rename(&leaf, &captured).expect("the leaf is captured under its private name");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&captured, std::fs::Permissions::from_mode(0o700))
                .expect("the captured leaf is writable");
        }
        let record = state::CaptureRecord {
            private,
            original: prepared.pending_intents()[0].capture_name().to_owned(),
            device: prepared.unit.device.get(),
            inode: prepared.unit.inode,
            kind: "directory".to_owned(),
            verdict: state::CaptureVerdict::Observed,
            mode: Some(0o040500),
        };
        publish_capture(&prepared, &record);
        assert_eq!(mode_of(&captured), 0o700);

        let replay = prepared.replay();
        assert_eq!(
            mode_of(&captured),
            0o500,
            "the captured leaf's recorded mode is restored from the area folder"
        );
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
    }

    #[test]
    fn a_leaf_swapped_before_its_final_removal_is_never_removed() {
        // The visible quarantine name is captured and verified again before the
        // leaf's removal. A racer that puts its own directory at that name gets its
        // directory put back, nothing is removed, and Barge records no completion.
        let prepared = prepared("delete-leaf-swap");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        let faults = Faults::new();
        faults.swapping_leaf(&area, QUARANTINE_PREFIX);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("not the recorded quarantine leaf"),
            "{:?}",
            report.reason
        );
        // The foreign directory is put back under the visible name and kept: an
        // empty directory put there is exactly what a removal by name would have
        // deleted and recorded as a completion.
        assert!(
            std::fs::metadata(&report.quarantine)
                .expect("the foreign directory is put back")
                .is_dir(),
            "the foreign directory is kept"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0].phase.is_pending(),
            "no completion is recorded for a leaf that was not the recorded one"
        );
        assert!(
            pending[0].phase < IntentPhase::Removed,
            "the removed phase is never reached"
        );
        assert_eq!(
            pending[0].foreign_captures.len(),
            1,
            "the foreign object at the quarantine name is recorded"
        );
        assert_eq!(
            pending[0].foreign_captures[0].put_back,
            state::PutBackOutcome::Renamed
        );
    }

    #[test]
    fn a_recorded_capture_that_vanishes_before_a_replay_keeps_it_pending() {
        // The intent records this operation's capture and the private name is gone.
        // Absence is not proof that Barge unlinked it, so the replay neither
        // completes the intent nor restores a mode it cannot reach.
        let prepared = prepared("replay-recorded-capture-gone");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "sub/nested/deep.txt", b"bytes");
        VolumeFixture::payload(&root, "sub/other.txt", b"bytes");
        let mut faults = Faults::new();
        faults.failing_facts_at("nested", 2);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        for private in capture_names(&leaf) {
            std::fs::remove_dir_all(&private).expect("another process moved the capture away");
        }
        assert!(capture_names(&leaf).is_empty());

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0]
                .reason
                .contains("gone without the removed phase"),
            "{:?}",
            replay.pending[0].reason
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the root is kept");
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert!(
            intents.records[0].phase.is_pending(),
            "the intent never settles on an absence"
        );
    }

    #[test]
    fn a_capture_swapped_before_its_replay_chmod_is_not_chmodded() {
        // The mode restoration re-verifies the descriptor it is about to chmod. A
        // racer that swaps the private name between the read and the open gets its
        // own directory left with its own mode, and the intent stays pending.
        let prepared = prepared("replay-mode-swap");
        let root = prepared.unit_path().clone();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
        }
        let faults = Faults::new();
        faults.failing_mode_restore();
        faults.failing_listing_after_mode();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        assert_eq!(capture_names(&leaf).len(), 1);

        let replay_faults = Faults::new();
        replay_faults.swapping_capture_on_open();
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let replay = replay_with(
            &prepared.fixture.state,
            &replay_faults,
            &prepared.fixture.facts(),
            &mounts,
            &observer,
            prepared.bounds(),
        )
        .expect("the replay reports");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0].reason.contains("not the recorded"),
            "{:?}",
            replay.pending[0].reason
        );
        // The racer's directory - the one at the exact private name, not the
        // `...moved` name it displaced - keeps the mode it was created with: the
        // restoration never chmods a descriptor it did not verify.
        let mut foreign = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&leaf) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.len() == CAPTURE_PREFIX.len() + 32 && name.starts_with(CAPTURE_PREFIX) {
                    foreign.push(entry.path());
                }
            }
        }
        assert_eq!(
            foreign.len(),
            1,
            "the racer's directory is at the private name"
        );
        assert_eq!(
            mode_of(&foreign[0]),
            0o755,
            "the racer's directory is not chmodded"
        );
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert!(intents.records[0].phase.is_pending());
    }

    #[test]
    fn a_mode_change_is_durable_before_the_chmod() {
        // The ordering proof for mode custody: at the moment the owner write and
        // search bits are applied, the intent already records the original mode, so
        // a crash in between still lets a replay restore it.
        let prepared = prepared("delete-mode-ordering");
        let root = prepared.unit_path().clone();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
        }
        let state_paths = prepared.fixture.state.clone();
        let faults = Faults::new();
        faults.probing_mode(Box::new(move |mode: u32| {
            if mode & 0o777 != 0o700 {
                return true;
            }
            let intents = state::read_intents(&state_paths, 64).expect("the journal reads");
            intents.records.iter().any(|record| {
                record
                    .captures
                    .iter()
                    .any(|capture| capture.mode == Some(0o040500))
            })
        }));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert_eq!(
            faults.mode_probe_answers().first().copied(),
            Some(true),
            "the recorded mode is durable before the chmod: {:?}",
            faults.mode_probe_answers()
        );
    }

    #[test]
    fn a_marker_last_replay_never_removes_a_swapped_leaf() {
        // The marker-last replay also captures and verifies the leaf before its
        // removal: a replacement put at the visible quarantine name is kept, the
        // intent stays pending and no completion is recorded.
        let prepared = prepared("replay-marker-last-leaf-swap");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        std::fs::remove_file(leaf.join(ROOT_MARKER_FILE)).expect("the marker is removed");
        std::fs::remove_file(leaf.join("payload.txt")).expect("the payload is removed");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);

        let faults = Faults::new();
        faults.swapping_leaf(&area, QUARANTINE_PREFIX);
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let replay = replay_with(
            &prepared.fixture.state,
            &faults,
            &prepared.fixture.facts(),
            &mounts,
            &observer,
            prepared.bounds(),
        )
        .expect("the replay reports");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0]
                .reason
                .contains("not the recorded quarantine leaf"),
            "{:?}",
            replay.pending[0].reason
        );
        // The racer's empty directory is put back under the visible name and kept.
        assert!(
            std::fs::metadata(&leaf)
                .expect("the foreign directory is put back")
                .is_dir(),
            "the foreign replacement is never removed"
        );
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::MarkerLast);
        assert_eq!(intents.records[0].foreign_captures.len(), 1);
    }

    #[test]
    fn a_marker_that_cannot_be_read_still_restores_the_recorded_mode_first() {
        // Rule 3 puts the mode restoration ahead of every other reconciliation
        // action, the marker read included: an unreadable marker must not skip it.
        let prepared = prepared("replay-marker-unreadable-mode");
        let root = prepared.unit_path().clone();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
        }
        let faults = Faults::new();
        faults.failing_mode_restore();
        faults.failing_listing_after_mode();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");

        // Make the marker unreadable, so a replay that read it before restoring
        // would stop with the mode still changed.
        let leaf = report.quarantine.clone();
        let marker = leaf.join(ROOT_MARKER_FILE);
        let bytes = std::fs::read(&marker).expect("the marker reads");
        let mut truncated = bytes.clone();
        truncated.truncate(bytes.len() / 2);
        std::fs::write(&marker, &truncated).expect("the marker is spoiled");
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1);

        let replay = prepared.replay();
        assert_eq!(
            mode_of(&private[0]),
            0o500,
            "the replay restored the recorded mode before it read the marker"
        );
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
    }

    #[test]
    fn a_recorded_capture_that_vanishes_between_its_listing_and_its_read_stays_pending() {
        // The walk lists the recorded capture, and another process moves it before
        // the read. The read answering "not there" is unknown, never a step that
        // completed, so the replay keeps the intent pending.
        let prepared = prepared("replay-capture-vanishes-mid-walk");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "sub/nested/deep.txt", b"bytes");
        let mut faults = Faults::new();
        faults.failing_facts_at("nested", 2);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        assert_eq!(capture_names(&leaf).len(), 1);

        let replay_faults = Faults::new();
        // The private-name reads in order: the mode-restoration walk, the
        // classification, then the delete pass's own read - which vanishes.
        replay_faults.vanishing_capture_at(3);
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let replay = replay_with(
            &prepared.fixture.state,
            &replay_faults,
            &prepared.fixture.facts(),
            &mounts,
            &observer,
            prepared.bounds(),
        )
        .expect("the replay reports");
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0].reason.contains("not there")
                || replay.pending[0]
                    .reason
                    .contains("recorded as this operation's capture"),
            "{:?}",
            replay.pending[0].reason
        );
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert!(intents.records[0].phase.is_pending());
        let _ = leaf;
    }

    #[test]
    fn a_replay_resumes_a_captured_nested_directory_and_finishes_the_delete() {
        // The crash lands after `sub` was captured and recorded, inside it. The
        // replay re-verifies the recorded capture, resumes it as the tree it was,
        // and finishes - recording `removed` before it settles.
        let prepared = prepared("replay-resume-nested");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "sub/nested/deep.txt", b"bytes");
        VolumeFixture::payload(&root, "sub/other.txt", b"bytes");
        let mut faults = Faults::new();
        faults.failing_facts_at("nested", 2);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        assert_eq!(
            capture_names(&leaf).len(),
            1,
            "the captured subdirectory is still there"
        );

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 1, "{replay:?}");
        assert!(replay.is_settled());
        assert!(!leaf.exists(), "the leaf is gone");
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::Deleted);
    }

    #[test]
    fn a_crash_between_the_actual_final_unlink_and_removed_stays_pending() {
        // The real transition: the final unlink happens, the `removed` publish does
        // not. The leaf is gone, and an absence without `removed` is Unknown, so a
        // replay keeps the intent pending.
        let prepared = prepared("crash-unlink-before-removed");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        state::fail_next_phase_publish(IntentPhase::Removed);
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::MarkerLast, "{report:?}");
        assert!(
            !report.quarantine.exists(),
            "the final unlink actually happened"
        );
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::MarkerLast);
        assert!(
            intents.records[0]
                .captures
                .iter()
                .any(|capture| capture.original == intents.records[0].capture_name()),
            "the leaf's own capture was recorded before it was removed"
        );

        let replay = prepared.replay();
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0]
                .reason
                .contains("removed phase was not recorded"),
            "{:?}",
            replay.pending[0].reason
        );
    }

    #[test]
    fn a_stop_after_marker_last_resumes_the_leaf_capture_on_replay() {
        // The real post-marker-last, pre-leaf-capture boundary: the marker is
        // unlinked and `marker_last` is durable, and the leaf capture fails. The
        // marker's completed capture is gone from the durable chain, so a replay
        // resumes the leaf capture instead of reading a missing capture as unknown.
        let prepared = prepared("stop-after-marker-last");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        let faults = Faults::new();
        faults.failing_leaf_capture();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::MarkerLast, "{report:?}");
        assert!(report.marker_removed, "the marker was unlinked last");
        assert!(report.quarantine.is_dir(), "the leaf is still there");
        assert!(
            !report.quarantine.join(ROOT_MARKER_FILE).exists(),
            "the marker is gone"
        );
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::MarkerLast);
        assert!(
            intents.records[0].captures.is_empty(),
            "the completed marker capture is not left in the durable chain: {:?}",
            intents.records[0].captures
        );

        let replay = prepared.replay();
        assert_eq!(replay.settled, 1, "{replay:?}");
        assert_eq!(replay.pending.len(), 0, "{replay:?}");
        assert!(replay.is_settled());
        assert!(!report.quarantine.exists(), "the leaf is gone");
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::Deleted);
    }

    #[test]
    fn a_crash_after_the_durable_removed_phase_settles_on_replay() {
        // The other side of the window: the final unlink happened and `removed` is
        // durable, so a replay completes when the leaf is positively absent.
        let prepared = prepared("crash-after-removed");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        state::fail_next_phase_publish(IntentPhase::Deleted);
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::Removed, "{report:?}");
        assert!(!report.quarantine.exists(), "the leaf is gone");
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::Removed);

        let replay = prepared.replay();
        assert_eq!(replay.settled, 1, "{replay:?}");
        assert!(replay.is_settled());
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::Deleted);
    }

    #[test]
    fn a_delete_records_the_removed_phase_before_it_settles() {
        // The durable completion evidence is written after Barge's own final unlink
        // and before the intent settles.
        let prepared = prepared("delete-removed-phase");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert_eq!(report.phase, IntentPhase::Deleted);
        let intents = state::read_intents(&prepared.fixture.state, 64).expect("the journal reads");
        assert_eq!(intents.records[0].phase, IntentPhase::Deleted);
        assert!(
            report.entries_removed >= 2,
            "the payload and the marker were unlinked: {report:?}"
        );
    }

    #[test]
    fn a_read_only_captured_directory_is_removed_and_its_mode_restored_on_abort() {
        // Mode custody: the recorded mode is applied to delete the contents, and
        // the guard puts it back when the walk aborts after the chmod.
        let prepared = prepared("delete-mode-abort");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
        }
        let faults = Faults::new();
        faults.failing_listing_after_mode();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(
            faults.modes_set().first().map(|mode| mode & 0o777),
            Some(0o700),
            "the deletion applied the owner write and search bits: {:?}",
            faults.modes_set()
        );
        let leaf = report.quarantine.clone();
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1, "the captured directory is still there");
        assert_eq!(
            mode_of(&private[0]),
            0o500,
            "the guard put the recorded mode back"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1, "the abort is recorded");
        assert_eq!(
            pending[0].captures[0].mode,
            Some(0o040500),
            "the record keeps the object's full st_mode"
        );
        let _ = area;
    }

    #[test]
    fn a_read_only_captured_directory_is_removed_whole() {
        // The recorded mode is applied so the contents can be unlinked, and the
        // whole root is deleted.
        let prepared = prepared("delete-read-only-directory");
        let root = prepared.unit_path().clone();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        VolumeFixture::payload(&locked, "deeper/item.bin", b"more");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
            std::fs::set_permissions(
                locked.join("deeper"),
                std::fs::Permissions::from_mode(0o500),
            )
            .expect("the nested directory is read-only");
        }
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert!(!root.exists(), "the whole root is gone");
        assert!(
            faults.modes_set().iter().any(|mode| mode & 0o777 == 0o700),
            "a read-only directory had its owner write and search bits applied: {:?}",
            faults.modes_set()
        );
    }

    #[test]
    fn a_read_only_root_directory_is_removed_whole() {
        // The quarantine leaf itself can be read-only: its mode is recorded before
        // the change, applied so the contents can be removed, and the deletion
        // completes.
        let prepared = prepared("delete-read-only-leaf");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))
                .expect("the root is read-only");
        }
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert!(!root.exists(), "the whole root is gone");
        assert!(
            faults.modes_set().iter().any(|mode| mode & 0o777 == 0o700),
            "the leaf's owner write and search bits were applied: {:?}",
            faults.modes_set()
        );
    }

    #[test]
    fn an_entry_read_error_after_the_chmod_restores_the_mode_and_records_the_abort() {
        // The second post-chmod failure seam: the abort lands on an entry read
        // inside the captured directory rather than on its listing, and the mode
        // custody is the same.
        let prepared = prepared("delete-mode-abort-entry-read");
        let root = prepared.unit_path().clone();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
        }
        let faults = Faults::new();
        faults.failing_facts_after_mode();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(
            faults.modes_set().first().map(|mode| mode & 0o777),
            Some(0o700),
            "the owner write and search bits were applied: {:?}",
            faults.modes_set()
        );
        let leaf = report.quarantine.clone();
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1, "the captured directory is still there");
        assert_eq!(
            mode_of(&private[0]),
            0o500,
            "the guard put the recorded mode back"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1, "the abort is recorded");
        assert_eq!(
            pending[0].captures[0].mode,
            Some(0o040500),
            "the record keeps the object's full st_mode"
        );
    }

    #[test]
    fn a_failed_mode_restore_stays_pending_and_a_replay_restores_it_first() {
        // A restore that fails leaves the intent pending with the mode recorded; a
        // replay restores the recorded mode before it does anything else.
        let prepared = prepared("delete-mode-restore");
        let root = prepared.unit_path().clone();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).expect("the directory exists");
        VolumeFixture::payload(&locked, "inside.txt", b"bytes");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
                .expect("the directory is read-only");
        }
        let faults = Faults::new();
        faults.failing_mode_restore();
        faults.failing_listing_after_mode();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1);
        assert_eq!(
            mode_of(&private[0]),
            0o700,
            "the failed restore left the recorded mode changed"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1, "the failed restore stays pending");
        assert_eq!(
            pending[0].captures[0].mode,
            Some(0o040500),
            "the object's full st_mode is recorded"
        );

        // A replay restores the recorded mode first. The marker is removed here so
        // the replay stops after the restore, which proves the order.
        std::fs::remove_file(leaf.join(ROOT_MARKER_FILE)).expect("the marker is removed");
        let replay = prepared.replay();
        assert_eq!(
            mode_of(&private[0]),
            0o500,
            "the replay restored the recorded mode first"
        );
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0].reason.contains("marker"),
            "{:?}",
            replay.pending[0].reason
        );
    }

    #[test]
    fn a_nested_mismatch_with_a_putback_collision_keeps_the_foreign_object_pending() {
        // Rule 2: a nested foreign capture is recorded before its put-back, a
        // put-back collision keeps it under its private name inside the captured
        // parent, and the root's abort never relabels it as an ordinary entry.
        let prepared = prepared("delete-nested-foreign-capture");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        VolumeFixture::payload(&root, "sub/nested.txt", b"mine");
        let foreign = b"the racer's object";
        let faults = Faults::new();
        faults.racing(
            &area,
            "sub/nested.txt",
            2,
            Replacement::File(foreign.to_vec()),
        );
        faults.occupying_after_capture("nested.txt");
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        let occupant = find_entry(&leaf, "nested.txt").expect("the taken name is kept");
        assert_eq!(
            std::fs::read(&occupant).expect("the occupant reads"),
            b"a racer took this name back",
            "the collided name is kept as the racer left it"
        );
        assert!(
            find_private_with(&leaf, foreign).is_some(),
            "a put-back collision keeps the foreign object under its private name"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        let recorded = &pending[0].foreign_captures;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].original, "nested.txt");
        assert_eq!(recorded[0].put_back, state::PutBackOutcome::Taken);
        assert!(
            pending[0].captures.len() <= 2,
            "the chain is the in-flight stack"
        );

        // The replay keeps it, and never treats it as content of the root.
        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            find_private_with(&leaf, foreign).is_some(),
            "the replay keeps the foreign object under its private name"
        );
    }

    #[test]
    fn an_unrecorded_private_name_planted_before_a_replay_is_never_unlinked() {
        let prepared = prepared("delete-unrecorded-private");
        let root = prepared.unit_path().clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        let planted = leaf.join(format!("{CAPTURE_PREFIX}{}", "ef".repeat(16)));
        std::fs::write(&planted, b"not this operation's capture").expect("the name is planted");

        let report = prepared.replay();
        assert_eq!(report.resumed, 0, "{report:?}");
        assert_eq!(report.settled, 0, "{report:?}");
        assert_eq!(report.pending.len(), 1, "{report:?}");
        assert!(
            report.pending[0].reason.contains("did not record"),
            "{:?}",
            report.pending[0].reason
        );
        assert_eq!(
            std::fs::read(&planted).expect("the planted name is never unlinked"),
            b"not this operation's capture"
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the root is kept");
    }

    #[test]
    fn the_mismatch_is_durable_before_the_object_is_put_back() {
        // The record is what protects the object, so it must be on disk before the
        // put-back returns the object to an ordinary name. The probe reads the
        // journal at the moment of the put-back, which is the window a crash would
        // lose if the order were the other way round.
        let prepared = prepared("delete-mismatch-before-put-back");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let state_paths = prepared.fixture.state.clone();
        let faults = Faults::new();
        faults.racing(
            &area,
            "payload.txt",
            2,
            Replacement::File(b"the racer's object".to_vec()),
        );
        faults.probing_put_back(Box::new(move |private: &str| {
            let intents = state::read_intents(&state_paths, 64).expect("the journal reads");
            intents.records.iter().any(|record| {
                record
                    .foreign_captures
                    .iter()
                    .any(|held| held.private == private)
            })
        }));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(
            faults.put_back_result(),
            Some(true),
            "the mismatch is durable before the put-back"
        );
        let leaf = report.quarantine.clone();
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the object was put back"),
            b"the racer's object"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].foreign_captures.len(), 1);
        assert_eq!(
            pending[0].foreign_captures[0].put_back,
            state::PutBackOutcome::Renamed,
            "the answer is recorded after the put-back"
        );
    }

    #[test]
    fn a_put_back_that_never_lands_keeps_the_object_private_and_the_intent_pending() {
        // The crash between the durable record and the put-back: the record is
        // there, the object is still under its private name, and nothing is
        // unlinked by the run or by a replay.
        let prepared = prepared("delete-mismatch-put-back-lost");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let foreign = b"the racer's object";
        let faults = Faults::new();
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        faults.failing_put_back();
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1, "the object stays under its private name");
        assert_eq!(
            std::fs::read(&private[0]).expect("the object is kept"),
            foreign
        );
        assert!(
            !leaf.join("payload.txt").exists(),
            "the put-back never landed"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].foreign_captures.len(), 1);
        assert_eq!(
            pending[0].foreign_captures[0].put_back,
            state::PutBackOutcome::Failed
        );

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert_eq!(
            std::fs::read(&private[0]).expect("the replay never unlinks it"),
            foreign
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file());
    }

    #[test]
    fn a_mismatch_that_cannot_be_recorded_does_not_put_the_object_back() {
        // If the record cannot be written, putting the object back would expose it
        // to a later walk with nothing to stop it. The deletion leaves it under its
        // private name instead - which no run and no replay ever unlinks - and
        // stops with the intent pending.
        use std::os::unix::fs::PermissionsExt;

        let prepared = prepared("delete-mismatch-unrecordable");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let foreign = b"the racer's object";
        let intents_dir = prepared.fixture.state.intents_dir();
        let original = std::fs::metadata(&intents_dir)
            .expect("the intents directory")
            .permissions();
        let closed = std::fs::Permissions::from_mode(0o500);
        let blocked = intents_dir.clone();
        let faults = Faults::new();
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        // The intent is already durable when the walk observes the entry, so this
        // closes the journal only for the mismatch record.
        faults.observing(Box::new(move |name: &str| {
            if name == "payload.txt" {
                let _ = std::fs::set_permissions(&blocked, closed.clone());
            }
        }));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        std::fs::set_permissions(&intents_dir, original).expect("the journal is writable again");

        assert!(report.is_pending(), "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("could not be recorded"),
            "{:?}",
            report.reason
        );
        let leaf = report.quarantine.clone();
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1, "the object stays under its private name");
        assert_eq!(
            std::fs::read(&private[0]).expect("the object is kept"),
            foreign
        );
        assert!(
            !leaf.join("payload.txt").exists(),
            "an unrecorded mismatch is never put back"
        );
        assert!(prepared.pending_intents()[0].foreign_captures.is_empty());

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert_eq!(
            std::fs::read(&private[0]).expect("the replay never unlinks it"),
            foreign
        );
    }

    #[test]
    fn a_mismatch_without_captured_facts_is_still_recorded_and_never_replayed() {
        // The parent binding failing is a mismatch whose captured facts cannot be
        // read: the record is written with no captured identity, and a replay
        // still refuses to resume.
        let prepared = prepared("delete-mismatch-no-captured-facts");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let foreign = b"the racer's object";
        let faults = Faults::new();
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        faults.shifting_parent(DeviceId::new(
            prepared.fixture.device.get().saturating_add(11),
        ));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        let recorded = &pending[0].foreign_captures;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert!(
            recorded[0].captured.is_none(),
            "the captured facts could not be read, and that is recorded"
        );
        assert!(
            recorded[0].reason.contains("parent") || recorded[0].reason.contains("directory"),
            "{:?}",
            recorded[0].reason
        );

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the replay never unlinks it"),
            foreign
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file());
    }

    #[test]
    fn replay_after_a_successful_put_back_keeps_the_foreign_object_and_the_intent() {
        // The review's second replay gap: a put-back that succeeds leaves the
        // captured object back under its original name, where an ordinary walk
        // finds it again. The mismatch is durable evidence, so a replay must not
        // resume and unlink the object Barge never observed.
        let prepared = prepared("replay-after-successful-put-back");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let foreign = b"the racer's object";
        let faults = Faults::new();
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("put back under its own name"),
            "{:?}",
            report.reason
        );
        let leaf = report.quarantine.clone();
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the put-back object"),
            foreign
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        let recorded = &pending[0].foreign_captures;
        assert_eq!(recorded.len(), 1, "the mismatch is recorded: {recorded:?}");
        assert_eq!(recorded[0].original, "payload.txt");
        assert_eq!(recorded[0].put_back, state::PutBackOutcome::Renamed);
        assert_ne!(
            recorded[0]
                .captured
                .as_ref()
                .expect("the record keeps what the captured object was")
                .inode,
            recorded[0].inode,
            "the record keeps the identity that did not match"
        );

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0].reason.contains("payload.txt"),
            "{:?}",
            replay.pending[0].reason
        );
        assert_eq!(
            std::fs::read(leaf.join("payload.txt"))
                .expect("the replay never unlinks the put-back object"),
            foreign
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the root is kept");
        assert_eq!(prepared.pending_intents().len(), 1);
    }

    #[test]
    fn finder_metadata_replaced_between_observation_and_capture_is_removed_by_class() {
        // A classified `.DS_Store` has no stable inode: Finder rebuilds it. When
        // the file under that name is replaced between the observation and the
        // capture, the capture is verified by class and the deletion continues.
        let prepared = prepared("delete-finder-classified-replaced");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        VolumeFixture::payload(&root, ".DS_Store", b"finder");
        let faults = Faults::new();
        faults.racing(
            &area,
            ".DS_Store",
            2,
            Replacement::File(b"finder rebuilt it".to_vec()),
        );
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_deleted(), "{:?}", report.reason);
        assert!(!root.exists(), "the whole root is gone");
        assert!(!report.quarantine.exists());
    }

    #[test]
    fn finder_metadata_over_the_size_bound_stops_the_delete() {
        // The class check keeps the brief's size bound: a recreated Finder file
        // that is too large is not Finder metadata, so the deletion stops.
        let prepared = prepared("delete-finder-too-large");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        VolumeFixture::payload(&root, ".DS_Store", b"finder");
        let oversized = vec![b'x'; usize::try_from(super::FINDER_METADATA_BYTES).unwrap_or(0) + 1];
        let faults = Faults::new();
        faults.racing(&area, ".DS_Store", 2, Replacement::File(oversized));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        assert!(
            std::fs::metadata(leaf.join(".DS_Store"))
                .expect("the oversized file is put back")
                .len()
                > super::FINDER_METADATA_BYTES,
            "the object is kept, never unlinked"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].foreign_captures.len(),
            1,
            "the mismatch is recorded"
        );
    }

    #[test]
    fn replay_after_a_put_back_collision_keeps_the_foreign_object_and_the_intent() {
        // The review's replay defect: a collision left a foreign object under a
        // private name. A replay must not reclassify it as an ordinary owned entry
        // and unlink it; it must keep it and stay pending.
        let prepared = prepared("replay-after-collision");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let foreign = b"the racer's object";
        let faults = Faults::new();
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        faults.occupying_after_capture("payload.txt");
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        let private = capture_names(&leaf);
        assert_eq!(private.len(), 1);
        assert_eq!(
            std::fs::read(&private[0]).expect("the captured object is kept"),
            foreign
        );
        let occupant = std::fs::read(leaf.join("payload.txt")).expect("the name that was taken");
        assert_eq!(occupant, b"a racer took this name back");

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0].reason.contains("payload.txt"),
            "the replay names the object it keeps: {:?}",
            replay.pending[0].reason
        );
        assert_eq!(
            std::fs::read(&private[0]).expect("the replay never unlinks the foreign object"),
            foreign
        );
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the occupant is kept too"),
            occupant
        );
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the root is kept");
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1, "the intent stays pending");
        assert_eq!(pending[0].foreign_captures.len(), 1);
    }

    #[test]
    fn replay_keeps_a_leftover_private_name_that_does_not_verify() {
        let prepared = prepared("replay-leftover-symlink");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        std::fs::remove_file(leaf.join(ROOT_MARKER_FILE)).expect("the marker is removed");
        let victim = area.join("victim.txt");
        std::fs::write(&victim, b"never removed").expect("the victim exists");
        // A private-shaped name that holds a symlink is not a captured object of
        // this deletion's, and a link is never followed or removed on sight.
        let private = leaf.join(format!("{CAPTURE_PREFIX}{}", "cd".repeat(16)));
        std::os::unix::fs::symlink(&victim, &private).expect("the link exists");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);

        let report = prepared.replay();
        assert_eq!(report.settled, 0, "{report:?}");
        assert_eq!(report.pending.len(), 1, "{report:?}");
        assert!(
            report.pending[0].reason.contains("symbolic link"),
            "{:?}",
            report.pending[0].reason
        );
        // The replay captured the leaf itself before walking it, so the leftover is
        // now inside the leaf's own private name.
        let kept = find_entry(
            &area,
            private
                .file_name()
                .expect("a name")
                .to_str()
                .expect("a name"),
        )
        .expect("the leftover is kept");
        assert!(
            std::fs::symlink_metadata(&kept)
                .expect("the leftover is kept")
                .file_type()
                .is_symlink(),
            "the leftover is still the link it was"
        );
        assert_eq!(
            std::fs::read(&victim).expect("the link target is untouched"),
            b"never removed"
        );
        let _ = leaf;
    }

    #[test]
    fn the_old_check_then_unlink_removes_every_raced_object_where_capture_verify_keeps_it() {
        // The counterfactual: for every successive entry, the same fixture and the
        // same racer, once through a check-then-unlink-by-name helper and once
        // through the deletion. The helper's check passes, the racer replaces the
        // entry, and the unlink by name removes the racer's object; the deletion
        // puts it back instead.
        let foreign = b"the racer's object";
        for (position, raced) in RACED_ENTRIES.iter().enumerate() {
            let old = prepared(&format!("delete-counterfactual-old-{position}"));
            let old_root = old.unit_path().clone();
            plant_tree(&old_root, b"mine");
            let old_dirs = Faults::new();
            old_dirs.racing(&old_root, raced, 1, Replacement::File(foreign.to_vec()));
            let handle = old_dirs.open_path(&old_root).expect("the root opens");
            let mut removed = 0;
            check_then_unlink(&old_dirs, &handle, &mut removed).expect("the old path runs");
            assert!(
                removed >= 1,
                "the old path unlinked by name at position {position}"
            );
            assert!(
                !old_root.join(raced).exists(),
                "`{raced}` (the racer's object) is gone, which is the defect"
            );

            let new = prepared(&format!("delete-counterfactual-new-{position}"));
            let new_root = new.unit_path().clone();
            plant_tree(&new_root, b"mine");
            let new_area = new_root.parent().expect("the area folder").to_path_buf();
            let new_dirs = Faults::new();
            new_dirs.racing(&new_area, raced, 2, Replacement::File(foreign.to_vec()));
            let facts = new.fixture.facts();
            let report = new.delete(&new_dirs, &facts);
            assert!(report.is_pending(), "{report:?}");
            let found = find_entry(&report.quarantine, raced_name(raced))
                .expect("the racer's object survives");
            assert_ne!(
                std::fs::read(&found).expect("the racer's object reads"),
                b"mine".to_vec(),
                "position {position} keeps the object the old path removed"
            );
        }
    }

    #[test]
    fn a_racer_that_replaces_every_successive_entry_gets_nothing_foreign_removed() {
        // The racer is registered for every entry, at three depths, and replaces
        // each one after its own check. The first replacement is a mismatch, so
        // the deletion stops: exactly one foreign object was created and it is
        // still there, and nothing was unlinked.
        let prepared = prepared("delete-racer-every-entry");
        let root = prepared.unit_path().clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        plant_tree(&root, b"mine");
        let faults = Faults::new();
        let mut foreign: Vec<Vec<u8>> = Vec::new();
        for (position, raced) in RACED_ENTRIES.iter().enumerate() {
            let bytes = format!("the racer's object {position}").into_bytes();
            faults.racing(&area, raced, 2, Replacement::File(bytes.clone()));
            foreign.push(bytes);
        }
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::Renamed, "{report:?}");
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
        assert!(!report.marker_removed, "the marker is kept");
        let leaf = report.quarantine.clone();
        let survivors: Vec<(&str, Vec<u8>)> = RACED_ENTRIES
            .iter()
            .map(|raced| {
                let found = find_entry(&leaf, raced_name(raced))
                    .expect("every entry is still there, under its own or a private parent");
                let bytes = std::fs::read(&found).expect("every entry reads");
                (*raced, bytes)
            })
            .collect();
        let replaced: Vec<&str> = survivors
            .iter()
            .filter(|(_, bytes)| foreign.contains(bytes))
            .map(|(raced, _)| *raced)
            .collect();
        assert_eq!(
            replaced.len(),
            1,
            "exactly one racer object was created, and it survived: {survivors:?}"
        );
        for (raced, bytes) in &survivors {
            if *raced != replaced[0] {
                assert_eq!(bytes, b"mine", "`{raced}` was never replaced");
            }
        }
        assert!(leaf.join(ROOT_MARKER_FILE).is_file(), "the root is kept");
        assert_eq!(prepared.pending_intents().len(), 1);
    }

    #[test]
    fn every_successive_entry_a_racer_replaces_is_kept_and_stops_the_delete() {
        // Each position in turn is the entry the racer replaces after its check:
        // the deletion stops there with the intent pending and the object stays
        // under its original name. No position lets a foreign object be unlinked.
        for (position, raced) in RACED_ENTRIES.iter().enumerate() {
            let prepared = prepared(&format!("delete-racer-successive-{position}"));
            let root = prepared.unit_path().clone();
            let area = root.parent().expect("the area folder").to_path_buf();
            plant_tree(&root, b"mine");
            let foreign = format!("the racer's object at {position}").into_bytes();
            let faults = Faults::new();
            faults.racing(&area, raced, 2, Replacement::File(foreign.clone()));
            let facts = prepared.fixture.facts();
            let report = prepared.delete(&faults, &facts);
            assert!(report.is_pending(), "position {position}: {report:?}");
            let leaf = report.quarantine.clone();
            let found = find_entry(&leaf, raced_name(raced))
                .expect("the racer's object survives under its own or its parent's private name");
            assert_eq!(
                std::fs::read(&found).expect("the racer's object reads"),
                foreign,
                "position {position} keeps the object it replaced"
            );
            for other in RACED_ENTRIES.iter().filter(|other| *other != raced) {
                if let Some(path) = find_entry(&leaf, raced_name(other)) {
                    let bytes = std::fs::read(&path).expect("the entry reads");
                    assert_eq!(
                        bytes,
                        b"mine".to_vec(),
                        "position {position}: `{other}` is either verified or untouched, never foreign"
                    );
                }
            }
            assert!(
                leaf.join(ROOT_MARKER_FILE).is_file(),
                "position {position} keeps the marker"
            );
            assert_eq!(
                prepared.pending_intents().len(),
                1,
                "position {position} leaves one pending intent"
            );
        }
    }

    // ---- every unit kind, through the one delete and replay path ------------

    /// One prepared fixture per unit kind a plan can name.
    fn unit_kind_cases(label: &str) -> Vec<(&'static str, Prepared)> {
        vec![
            ("barge", prepared(&format!("{label}-barge"))),
            (
                "effigy",
                prepared_foreign(&format!("{label}-effigy"), MarkerFamily::EffigyScratch),
            ),
            (
                "oxide",
                prepared_foreign(&format!("{label}-oxide"), MarkerFamily::OxideScratch),
            ),
            ("unmarked", prepared_unmarked(&format!("{label}-unmarked"))),
            ("file", prepared_file(&format!("{label}-file"))),
        ]
    }

    /// One way to prepare a unit of a named kind.
    type UnitMaker = (&'static str, fn(&str) -> Prepared);

    /// The makers of one unit kind each, so a per-kind proof runs the same test
    /// body over every kind the delete path must handle identically.
    fn unit_kind_makers() -> Vec<UnitMaker> {
        vec![
            ("barge", prepared as fn(&str) -> Prepared),
            ("effigy", |label: &str| {
                prepared_foreign(label, MarkerFamily::EffigyScratch)
            }),
            ("oxide", |label: &str| {
                prepared_foreign(label, MarkerFamily::OxideScratch)
            }),
            ("unmarked", prepared_unmarked),
            ("file", prepared_file),
        ]
    }

    /// The makers of one directory unit kind each, for the proofs that only a
    /// directory's contents and leaf go through the walk.
    fn directory_kind_makers() -> Vec<UnitMaker> {
        vec![
            ("barge", prepared as fn(&str) -> Prepared),
            ("effigy", |label: &str| {
                prepared_foreign(label, MarkerFamily::EffigyScratch)
            }),
            ("oxide", |label: &str| {
                prepared_foreign(label, MarkerFamily::OxideScratch)
            }),
            ("unmarked", prepared_unmarked),
        ]
    }

    /// Plant the payloads a directory unit carries; a file unit is its own only
    /// content.
    fn plant_unit_contents(prepared: &Prepared) {
        if prepared.unit.kind == UnitKind::Directory {
            prepared.payload("payload.txt", b"mine");
            prepared.payload("sub/nested.bin", b"mine too");
        }
    }

    /// What the `marker_last` phase records: the contents and, when the unit
    /// records one, the marker are gone and only the captured leaf is left.
    fn settle_leaf_contents(prepared: &Prepared, leaf: &Path) {
        if prepared.unit.kind == UnitKind::File {
            return;
        }
        std::fs::remove_dir_all(leaf.join("sub")).expect("the nested directory is removed");
        std::fs::remove_file(leaf.join("payload.txt")).expect("the payload is removed");
        if let Some(marker) = prepared.unit.marker_file() {
            std::fs::remove_file(leaf.join(marker)).expect("the marker is removed last");
        }
    }

    /// Remove the captured leaf whole, which is the state the `removed` phase
    /// records.
    fn remove_leaf_whole(prepared: &Prepared, leaf: &Path) {
        if prepared.unit.kind == UnitKind::File {
            std::fs::remove_file(leaf).expect("the file leaf is removed");
        } else {
            std::fs::remove_dir_all(leaf).expect("the directory leaf is removed");
        }
    }

    #[test]
    fn every_unit_kind_is_deleted_whole_through_the_one_path() {
        for (label, prepared) in unit_kind_cases("delete-kind") {
            plant_unit_contents(&prepared);
            let faults = Faults::new();
            let facts = prepared.fixture.facts();
            let report = prepared.delete(&faults, &facts);
            assert!(report.is_deleted(), "{label}: {:?}", report.reason);
            assert_eq!(report.unit_kind, prepared.unit.kind.as_str(), "{label}");
            assert_eq!(report.unit_name, prepared.unit.name, "{label}");
            assert_eq!(report.parent, prepared.unit.parent, "{label}");
            assert!(
                !prepared.unit_path().exists(),
                "{label}: the whole unit is gone"
            );
            assert!(
                !report.quarantine.exists(),
                "{label}: nothing is quarantined"
            );
            assert_eq!(
                report.marker_removed,
                prepared.unit.marker.is_some(),
                "{label}: a recorded marker is removed last"
            );
            let evidence = report.custody.as_ref().expect("the evidence");
            assert_eq!(evidence.unit_kind, prepared.unit.kind.as_str(), "{label}");
            assert_eq!(evidence.grant, prepared.unit.grant, "{label}");
            let expected_grant = match prepared.unit.marker.as_ref().map(|marker| marker.family) {
                Some(MarkerFamily::Barge) => GrantKind::NativeRoot,
                Some(_) => GrantKind::Delegation,
                None => GrantKind::Claim,
            };
            assert_eq!(
                prepared.unit.grant.kind, expected_grant,
                "{label}: the grant is the form its marker family needs"
            );
            assert_eq!(evidence.marker, prepared.unit.marker, "{label}");
            assert!(
                prepared.pending_intents().is_empty(),
                "{label}: the intent settled"
            );
            if prepared.unit.kind == UnitKind::File {
                assert_eq!(report.entries_removed, 1, "{label}");
                assert_eq!(report.directories_removed, 0, "{label}");
            } else {
                assert!(report.entries_removed >= 2, "{label}: {report:?}");
                assert!(report.directories_removed >= 2, "{label}: {report:?}");
            }
        }
    }

    #[test]
    fn a_racer_that_replaces_each_entry_keeps_every_foreign_object_for_each_kind() {
        // The Barge root's own racer proofs exist above; the same rule is what
        // every other unit kind gets, because they all go through the one path.
        for (label, prepared) in unit_kind_cases("delete-racer-kind") {
            plant_unit_contents(&prepared);
            let area = prepared.parent_path();
            let faults = Faults::new();
            let foreign = b"the racer's object".to_vec();
            if prepared.unit.kind == UnitKind::File {
                // The racer replaces the file between its check and the quarantine
                // rename, so the object the rename carries is the foreign one.
                faults.racing(
                    &area,
                    &prepared.unit.name,
                    2,
                    Replacement::File(foreign.clone()),
                );
            } else {
                faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.clone()));
            }
            let facts = prepared.fixture.facts();
            let report = prepared.delete(&faults, &facts);
            assert!(report.is_pending(), "{label}: {report:?}");
            let leaf = report.quarantine.clone();
            if prepared.unit.kind == UnitKind::File {
                // The file's identity moved between the plan and the effect, so the
                // re-decided custody refuses before the rename: the racer's file is
                // still at its own name and nothing was unlinked.
                assert_eq!(report.phase, IntentPhase::Intent, "{label}");
                assert_eq!(report.entries_removed, 0, "{label}: nothing was unlinked");
                assert!(report.custody.is_some(), "{label}");
                assert!(!leaf.exists(), "{label}: nothing was renamed");
                assert_eq!(
                    std::fs::read(prepared.unit_path()).expect("the foreign file is kept"),
                    foreign,
                    "{label}: the object that replaced the observed one is never unlinked"
                );
            } else {
                assert_eq!(report.phase, IntentPhase::Renamed, "{label}");
                let found = find_entry(&leaf, "payload.txt")
                    .expect("{label}: the racer's object survives under its own name");
                assert_eq!(
                    std::fs::read(&found).expect("the kept object reads"),
                    foreign,
                    "{label}: the object that was not the observed one is never unlinked"
                );
                assert!(
                    find_private_with(&leaf, &foreign).is_none(),
                    "{label}: it is put back, not left private"
                );
                assert!(capture_names(&leaf).is_empty(), "{label}");
            }
            let pending = prepared.pending_intents();
            assert_eq!(pending.len(), 1, "{label}: the intent stays pending");
            assert_eq!(
                pending[0].foreign_captures.len(),
                usize::from(prepared.unit.kind != UnitKind::File),
                "{label}"
            );

            // A replay of that pending intent completes nothing and removes
            // nothing foreign.
            let replay = prepared.replay();
            assert_eq!(replay.settled, 0, "{label}: {replay:?}");
            assert_eq!(replay.resumed, 0, "{label}: {replay:?}");
            assert_eq!(replay.pending.len(), 1, "{label}: {replay:?}");
            if prepared.unit.kind == UnitKind::File {
                assert_eq!(
                    std::fs::read(prepared.unit_path()).expect("the foreign file is kept"),
                    foreign,
                    "{label}: the replay never unlinks it"
                );
            } else {
                let found = find_entry(&leaf, "payload.txt")
                    .expect("{label}: the racer's object is still there");
                assert_eq!(
                    std::fs::read(&found).expect("the kept object reads"),
                    foreign,
                    "{label}: the replay never unlinks it"
                );
                assert!(leaf.is_dir(), "{label}: the stopped leaf is kept");
            }
        }
    }

    #[test]
    fn every_unit_kind_replays_from_every_phase() {
        for (label, make) in unit_kind_makers() {
            // Before the capture: the intent is written and the unit is still in
            // place, so a replay rolls it back and touches nothing.
            let rolled = make(&format!("replay-intent-{label}"));
            plant_unit_contents(&rolled);
            publish(&rolled, &[]);
            let replay = rolled.replay();
            assert_eq!(replay.rolled_back, 1, "{label}: {replay:?}");
            assert!(replay.is_settled(), "{label}: {replay:?}");
            assert!(
                rolled.unit_path().exists(),
                "{label}: the unit is untouched"
            );
            assert!(rolled.pending_intents().is_empty(), "{label}");

            // After the rename: a replay resumes the deletion and completes it.
            let renamed = make(&format!("replay-renamed-{label}"));
            plant_unit_contents(&renamed);
            publish(&renamed, &[IntentPhase::Renamed]);
            rename_to_quarantine(&renamed);
            let replay = renamed.replay();
            assert_eq!(replay.resumed, 1, "{label}: {replay:?}");
            assert_eq!(replay.settled, 0, "{label}: a resumed deletion is resumed");
            assert!(replay.is_settled(), "{label}: {replay:?}");
            assert!(!renamed.unit_path().exists(), "{label}");
            assert!(!renamed.quarantine().exists(), "{label}");
            assert!(renamed.pending_intents().is_empty(), "{label}");

            // Mid-delete, at the marker-last phase: the contents and the marker
            // are gone and only the captured leaf is left.
            let marker_last = make(&format!("replay-marker-{label}"));
            plant_unit_contents(&marker_last);
            publish(
                &marker_last,
                &[IntentPhase::Renamed, IntentPhase::MarkerLast],
            );
            rename_to_quarantine(&marker_last);
            let leaf = marker_last.quarantine();
            settle_leaf_contents(&marker_last, &leaf);
            let replay = marker_last.replay();
            assert_eq!(replay.settled, 1, "{label}: {replay:?}");
            assert_eq!(replay.resumed, 0, "{label}: the leaf removal settles it");
            assert!(replay.is_settled(), "{label}: {replay:?}");
            assert!(!marker_last.quarantine().exists(), "{label}");
            assert!(marker_last.pending_intents().is_empty(), "{label}");

            // After the durable removed phase: an absent leaf settles, and
            // nothing is touched.
            let removed = make(&format!("replay-removed-{label}"));
            plant_unit_contents(&removed);
            publish(
                &removed,
                &[
                    IntentPhase::Renamed,
                    IntentPhase::MarkerLast,
                    IntentPhase::Removed,
                ],
            );
            rename_to_quarantine(&removed);
            remove_leaf_whole(&removed, &removed.quarantine());
            let replay = removed.replay();
            assert_eq!(replay.settled, 1, "{label}: {replay:?}");
            assert!(replay.is_settled(), "{label}: {replay:?}");
            assert!(removed.pending_intents().is_empty(), "{label}");
        }
    }

    #[test]
    fn a_read_only_unit_of_every_directory_kind_is_removed_whole() {
        use std::os::unix::fs::PermissionsExt;
        let makers: Vec<UnitMaker> = vec![
            ("barge", prepared as fn(&str) -> Prepared),
            ("effigy", |label: &str| {
                prepared_foreign(label, MarkerFamily::EffigyScratch)
            }),
            ("unmarked", prepared_unmarked),
        ];
        for (label, make) in makers {
            let prepared = make(&format!("read-only-{label}"));
            plant_unit_contents(&prepared);
            std::fs::set_permissions(prepared.unit_path(), std::fs::Permissions::from_mode(0o500))
                .expect("the unit is made read-only");
            let faults = Faults::new();
            let report = prepared.delete(&faults, &prepared.fixture.facts());
            assert!(report.is_deleted(), "{label}: {:?}", report.reason);
            assert!(!prepared.unit_path().exists(), "{label}");
            assert!(
                faults.modes_set().iter().any(|mode| mode & 0o300 == 0o300),
                "{label}: the owner write and search bits were added to remove the contents"
            );
        }
    }

    #[test]
    fn an_aborted_unit_of_every_directory_kind_restores_its_recorded_mode_then_replays() {
        use std::os::unix::fs::PermissionsExt;
        for (label, make) in directory_kind_makers() {
            let prepared = make(&format!("mode-abort-{label}"));
            plant_unit_contents(&prepared);
            std::fs::set_permissions(prepared.unit_path(), std::fs::Permissions::from_mode(0o500))
                .expect("the unit is made read-only");
            let faults = Faults::new();
            faults.failing_listing_after_mode();
            let report = prepared.delete(&faults, &prepared.fixture.facts());
            assert!(report.is_pending(), "{label}: {report:?}");
            assert_eq!(report.phase, IntentPhase::Renamed, "{label}");
            assert_eq!(
                mode_of(&report.quarantine),
                0o500,
                "{label}: the recorded mode is put back on the captured leaf"
            );
            let pending = prepared.pending_intents();
            assert!(
                pending[0]
                    .captures
                    .iter()
                    .any(|capture| capture.mode == Some(0o040500)),
                "{label}: the original full st_mode is recorded: {:?}",
                pending[0].captures
            );
            // A replay restores the recorded mode first and then finishes the
            // delete.
            let replay = prepared.replay();
            assert_eq!(replay.resumed, 1, "{label}: {replay:?}");
            assert!(replay.is_settled(), "{label}: {replay:?}");
            assert!(
                !prepared.unit_path().exists(),
                "{label}: the unit is deleted whole"
            );
        }
    }

    #[test]
    fn every_successive_entry_a_racer_replaces_is_kept_for_every_directory_kind() {
        // The Barge-root proof runs over all three depths above. Every other
        // directory kind goes through the same walk, so the same racer must leave
        // every foreign object it creates in place, at every position, for each of
        // them too.
        for (kind_label, make) in directory_kind_makers() {
            for (position, raced) in RACED_ENTRIES.iter().enumerate() {
                let prepared = make(&format!("racer-each-{kind_label}-{position}"));
                plant_tree(&prepared.unit_path(), b"mine");
                let holder = prepared.parent_path();
                let foreign = format!("the racer's object at {kind_label} {position}").into_bytes();
                let faults = Faults::new();
                faults.racing(&holder, raced, 2, Replacement::File(foreign.clone()));
                let report = prepared.delete(&faults, &prepared.fixture.facts());
                assert!(
                    report.is_pending(),
                    "{kind_label} at {position}: {report:?}"
                );
                let leaf = report.quarantine.clone();
                let found = find_entry(&leaf, raced_name(raced)).unwrap_or_else(|| {
                    panic!("{kind_label} at {position}: the racer's object survives")
                });
                assert_eq!(
                    std::fs::read(&found).expect("the racer's object reads"),
                    foreign,
                    "{kind_label} at {position}: the object it replaced is kept"
                );
                for other in RACED_ENTRIES.iter().filter(|other| *other != raced) {
                    if let Some(path) = find_entry(&leaf, raced_name(other)) {
                        assert_eq!(
                            std::fs::read(&path).expect("the entry reads"),
                            b"mine".to_vec(),
                            "{kind_label} at {position}: `{other}` is never foreign"
                        );
                    }
                }
                if let Some(marker) = prepared.unit.marker_file() {
                    assert!(
                        leaf.join(marker).is_file(),
                        "{kind_label} at {position}: the marker is kept"
                    );
                }
                let pending = prepared.pending_intents();
                assert_eq!(
                    pending.len(),
                    1,
                    "{kind_label} at {position}: one pending intent"
                );
                assert_eq!(
                    pending[0].foreign_captures.len(),
                    1,
                    "{kind_label} at {position}: the mismatch is recorded"
                );
            }
        }
    }

    #[test]
    fn a_leaf_replaced_before_its_final_capture_is_never_removed_for_every_kind() {
        // The final capture window: a racer replaces what is at the visible private
        // name after the deletion verified it there. A directory leaf is swapped
        // for an empty foreign directory; a file is replaced by a new inode. In both
        // cases the recorded identity no longer matches, so nothing is unlinked and
        // the intent stays pending.
        for (kind_label, make) in unit_kind_makers() {
            let prepared = make(&format!("leaf-swap-{kind_label}"));
            plant_unit_contents(&prepared);
            let faults = Faults::new();
            if prepared.unit.kind == UnitKind::File {
                // The file is replaced after the deletion's own read of it and before
                // the final capture, so the capture never sees the recorded object.
                // The intent id is fresh per run, so the probe uses the name it is
                // handed rather than a known private name.
                let parent = prepared.parent_path();
                let swapped = std::cell::Cell::new(false);
                faults.observing(Box::new(move |name| {
                    if swapped.get() || !name.starts_with(QUARANTINE_PREFIX) {
                        return;
                    }
                    let leaf = parent.join(name);
                    if !leaf.is_file() {
                        return;
                    }
                    std::fs::remove_file(&leaf)
                        .and_then(|()| std::fs::write(&leaf, b"the racer's leaf"))
                        .expect("the racer replaces the file leaf");
                    swapped.set(true);
                }));
            } else {
                // The leaf is empty by now (contents and marker gone), so the
                // racer can put its own empty directory at the visible name.
                faults.swapping_leaf(&prepared.parent_path(), QUARANTINE_PREFIX);
            }
            let report = prepared.delete(&faults, &prepared.fixture.facts());
            assert!(report.is_pending(), "{kind_label}: {report:?}");
            // A file leaf's replacement is caught by the finish check before the
            // marker-last phase; a directory leaf is swapped at its own final
            // capture, after the phase.
            let expected_phase = if prepared.unit.kind == UnitKind::File {
                IntentPhase::Renamed
            } else {
                IntentPhase::MarkerLast
            };
            assert_eq!(report.phase, expected_phase, "{kind_label}");
            let leaf = report.quarantine.clone();
            assert!(
                leaf.exists(),
                "{kind_label}: the replacement is kept at the visible name"
            );
            if prepared.unit.kind == UnitKind::File {
                assert_eq!(
                    std::fs::read(&leaf).expect("the racer's file is kept"),
                    b"the racer's leaf",
                    "{kind_label}: the final capture never unlinks it"
                );
            } else {
                assert!(leaf.is_dir(), "{kind_label}: the racer's directory is kept");
                assert_eq!(
                    prepared.pending_intents()[0].foreign_captures.len(),
                    1,
                    "{kind_label}: the mismatch is recorded"
                );
            }
            // A replay keeps it too: nothing records the removed phase.
            let replay = prepared.replay();
            assert_eq!(replay.settled, 0, "{kind_label}: {replay:?}");
            assert_eq!(replay.resumed, 0, "{kind_label}: {replay:?}");
            assert_eq!(replay.pending.len(), 1, "{kind_label}: {replay:?}");
            assert!(
                leaf.exists(),
                "{kind_label}: the replacement is still there"
            );
        }
    }

    #[test]
    fn a_file_leaf_replaced_before_its_final_capture_is_recorded_and_never_deleted() {
        // The file unit's final-capture window: the leaf passed its finish check,
        // and a racer replaces it after that read and before Barge captures it. The
        // capture then holds a foreign object, so it is recorded as a foreign
        // capture **before** its put-back, put back under the visible name, and the
        // intent stops pending. A replay reads that record and unlinks nothing.
        let prepared = prepared_file("file-final-capture-racer");
        let parent = prepared.parent_path();
        let reads = std::cell::Cell::new(0usize);
        let swapped = std::cell::Cell::new(false);
        let faults = Faults::new();
        faults.observing(Box::new(move |name| {
            if !name.starts_with(QUARANTINE_PREFIX) {
                return;
            }
            reads.set(reads.get().saturating_add(1));
            // Read one is the post-rename leaf check; read two is the finish
            // check, so the replacement lands exactly between it and the final
            // capture.
            if reads.get() != 2 || swapped.get() {
                return;
            }
            let leaf = parent.join(name);
            if leaf.is_file() {
                std::fs::remove_file(&leaf)
                    .and_then(|()| std::fs::write(&leaf, b"the racer's file"))
                    .expect("the racer replaces the file leaf");
                swapped.set(true);
            }
        }));
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::MarkerLast, "{report:?}");
        assert_eq!(report.entries_removed, 0, "nothing was unlinked");
        assert!(!report.marker_removed);
        let leaf = report.quarantine.clone();
        assert_eq!(
            std::fs::read(&leaf).expect("the foreign file is kept"),
            b"the racer's file",
            "the object the final capture found is never unlinked"
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1, "the intent stays pending");
        assert_eq!(pending[0].phase, IntentPhase::MarkerLast);
        let foreign = &pending[0].foreign_captures;
        assert_eq!(
            foreign.len(),
            1,
            "the final capture of a foreign object is recorded before its put-back: {foreign:?}"
        );
        assert_eq!(
            foreign[0].original,
            pending[0].capture_name(),
            "the record names the captured leaf it came from"
        );
        assert_eq!(foreign[0].kind, "file");
        assert_eq!(
            foreign[0].put_back,
            state::PutBackOutcome::Renamed,
            "the foreign file went back under its own name"
        );
        assert_ne!(
            foreign[0].captured.as_ref().map(|captured| captured.inode),
            Some(pending[0].unit.inode),
            "the record keeps the identity that did not match"
        );

        // A replay reads the foreign record, refuses to resume, and deletes
        // nothing: the foreign file capture survives both the run and the replay.
        let replay = prepared.replay();
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0].reason.contains("did not verify"),
            "the replay names the unverified capture: {:?}",
            replay.pending[0].reason
        );
        assert_eq!(
            std::fs::read(&leaf).expect("the replay keeps the foreign file"),
            b"the racer's file"
        );
        let after = prepared.pending_intents();
        assert_eq!(after[0].phase, IntentPhase::MarkerLast, "never settled");
        assert_eq!(after[0].foreign_captures.len(), 1, "the record is kept");

        // A second replay is just as conservative.
        let again = prepared.replay();
        assert_eq!(again.pending.len(), 1, "{again:?}");
        assert_eq!(
            std::fs::read(&leaf).expect("the foreign file is still there"),
            b"the racer's file"
        );
    }

    #[test]
    fn a_captured_leaf_that_vanishes_before_its_final_unlink_stays_pending() {
        // The crash window #014 names: Barge captured the leaf, verified it, and
        // then the unlink answered "already gone". That is never a completion.
        for (kind_label, make) in [
            ("unmarked", prepared_unmarked as fn(&str) -> Prepared),
            ("file", prepared_file),
        ] {
            let prepared = make(&format!("leaf-vanish-{kind_label}"));
            let faults = Faults::new();
            faults.vanishing_capture();
            let report = prepared.delete(&faults, &prepared.fixture.facts());
            assert!(report.is_pending(), "{kind_label}: {report:?}");
            assert_eq!(report.phase, IntentPhase::MarkerLast, "{kind_label}");
            assert_eq!(report.entries_removed, 0, "{kind_label}");
            assert!(
                report
                    .reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("vanished before Barge"),
                "{kind_label}: {:?}",
                report.reason
            );
            let pending = prepared.pending_intents();
            assert_eq!(pending.len(), 1, "{kind_label}");
            assert_eq!(pending[0].phase, IntentPhase::MarkerLast, "{kind_label}");
            // An absent leaf without the durable `removed` phase is unknown, so a
            // replay never completes it.
            let replay = prepared.replay();
            assert_eq!(replay.settled, 0, "{kind_label}: {replay:?}");
            assert_eq!(replay.pending.len(), 1, "{kind_label}: {replay:?}");
            assert!(
                replay.pending[0]
                    .reason
                    .contains("removed phase was not recorded"),
                "{kind_label}: {:?}",
                replay.pending[0].reason
            );
        }
    }

    #[test]
    fn a_resumed_delete_finishes_a_recorded_capture_for_every_directory_kind() {
        // The mid-delete crash: one entry had been captured, verified and recorded
        // under its private name, and the run stopped before its unlink. A replay
        // re-verifies that record and removes only it, then finishes the unit.
        for (kind_label, make) in directory_kind_makers() {
            let prepared = make(&format!("replay-in-flight-{kind_label}"));
            plant_unit_contents(&prepared);
            publish(&prepared, &[IntentPhase::Renamed]);
            rename_to_quarantine(&prepared);
            let leaf = prepared.quarantine();
            let payload = leaf.join("payload.txt");
            let facts = prepared
                .fixture
                .platform()
                .path_facts(&payload)
                .expect("the entry reads")
                .expect("the entry is there");
            let private = format!("{CAPTURE_PREFIX}{}", "ab".repeat(16));
            std::fs::rename(&payload, leaf.join(&private)).expect("the entry is captured");
            publish_capture(
                &prepared,
                &state::CaptureRecord {
                    private,
                    original: "payload.txt".to_owned(),
                    device: facts.device.get(),
                    inode: facts.inode,
                    kind: "file".to_owned(),
                    verdict: state::CaptureVerdict::Observed,
                    mode: None,
                },
            );

            assert_eq!(
                prepared.pending_intents()[0].captures.len(),
                1,
                "{kind_label}: the capture record is durable before the replay"
            );
            let replay = prepared.replay();
            assert_eq!(replay.resumed, 1, "{kind_label}: {replay:?}");
            assert!(replay.is_settled(), "{kind_label}: {replay:?}");
            assert!(
                !prepared.unit_path().exists(),
                "{kind_label}: the unit is deleted whole"
            );
            assert!(
                !prepared.quarantine().exists(),
                "{kind_label}: the recorded capture and the leaf are gone"
            );
            assert!(prepared.pending_intents().is_empty(), "{kind_label}");
        }
    }

    #[test]
    fn a_replay_keeps_a_recorded_capture_it_has_not_reached_in_every_snapshot() {
        // A snapshot published for another capture must keep the captures the
        // intent already records and the replay has not reached: they are still
        // under their private names, and their record is the only thing that
        // authorises finishing them. The leaf's own mode record makes the first
        // snapshot happen before the walk, so the stop is deterministic.
        use std::os::unix::fs::PermissionsExt;
        let prepared = prepared_unmarked("replay-keeps-unreached");
        plant_unit_contents(&prepared);
        publish(&prepared, &[IntentPhase::Renamed]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        let payload = leaf.join("payload.txt");
        let facts = prepared
            .fixture
            .platform()
            .path_facts(&payload)
            .expect("the entry reads")
            .expect("the entry is there");
        let private = format!("{CAPTURE_PREFIX}{}", "ab".repeat(16));
        std::fs::rename(&payload, leaf.join(&private)).expect("the entry is captured");
        publish_capture(
            &prepared,
            &state::CaptureRecord {
                private: private.clone(),
                original: "payload.txt".to_owned(),
                device: facts.device.get(),
                inode: facts.inode,
                kind: "file".to_owned(),
                verdict: state::CaptureVerdict::Observed,
                mode: None,
            },
        );

        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o500))
            .expect("the captured leaf is made read-only");
        let faults = Faults::new();
        faults.failing_listing_after_mode();
        let replay = replay_with(
            &prepared.fixture.state,
            &faults,
            &prepared.fixture.facts(),
            &prepared.fixture.mounts(),
            &prepared.fixture.observer(),
            prepared.bounds(),
        )
        .expect("the replay reports");
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        let pending = prepared.pending_intents();
        assert!(
            pending[0]
                .captures
                .iter()
                .any(|capture| capture.private == private),
            "the snapshot keeps the unreached recorded capture: {:?}",
            pending[0].captures
        );
        assert!(
            pending[0]
                .captures
                .iter()
                .any(|capture| capture.private == pending[0].capture_name()),
            "the leaf's own mode record is in the same snapshot: {:?}",
            pending[0].captures
        );
        assert!(
            leaf.join(&private).is_file(),
            "nothing unlinks the recorded capture"
        );
    }

    #[test]
    fn a_file_leaf_captured_before_its_unlink_stays_pending_for_a_replay() {
        // The leaf is the one capture a replay never resumes from a private name:
        // only the durable `removed` phase proves Barge removed it. A crash between
        // a file leaf's capture and its unlink therefore stays pending, and the
        // recorded private object is never unlinked.
        let prepared = prepared_file("replay-file-leaf-captured");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);
        rename_to_quarantine(&prepared);
        let leaf = prepared.quarantine();
        let facts = prepared
            .fixture
            .platform()
            .path_facts(&leaf)
            .expect("the leaf reads")
            .expect("the leaf is there");
        let parent = prepared.parent_path();
        let private = format!("{CAPTURE_PREFIX}{}", "cd".repeat(16));
        std::fs::rename(&leaf, parent.join(&private)).expect("the leaf is captured");
        publish_capture(
            &prepared,
            &state::CaptureRecord {
                private: private.clone(),
                original: format!("{QUARANTINE_PREFIX}{:032x}", 1),
                device: facts.device.get(),
                inode: facts.inode,
                kind: "file".to_owned(),
                verdict: state::CaptureVerdict::Observed,
                mode: None,
            },
        );

        let replay = prepared.replay();
        assert_eq!(replay.settled, 0, "{replay:?}");
        assert_eq!(replay.resumed, 0, "{replay:?}");
        assert_eq!(replay.pending.len(), 1, "{replay:?}");
        assert!(
            replay.pending[0]
                .reason
                .contains("removed phase was not recorded"),
            "{:?}",
            replay.pending[0].reason
        );
        assert_eq!(
            std::fs::read(parent.join(&private)).expect("the recorded capture is kept"),
            b"bytes",
            "a replay never unlinks a captured leaf from its private name"
        );
        assert_eq!(prepared.pending_intents()[0].phase, IntentPhase::MarkerLast);
    }

    #[test]
    fn a_leaf_absent_without_the_removed_phase_stays_unknown_for_every_kind() {
        // The crash between Barge's own final unlink and the durable `removed`
        // phase: an absence alone never resolves an intent, for any kind.
        for (label, make) in unit_kind_makers() {
            let prepared = make(&format!("crash-removed-{label}"));
            plant_unit_contents(&prepared);
            publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);
            rename_to_quarantine(&prepared);
            remove_leaf_whole(&prepared, &prepared.quarantine());
            let replay = prepared.replay();
            assert_eq!(replay.settled, 0, "{label}: {replay:?}");
            assert_eq!(replay.resumed, 0, "{label}: {replay:?}");
            assert_eq!(replay.pending.len(), 1, "{label}: {replay:?}");
            assert!(
                replay.pending[0]
                    .reason
                    .contains("removed phase was not recorded"),
                "{label}: {:?}",
                replay.pending[0].reason
            );
            assert_eq!(
                prepared.pending_intents()[0].phase,
                IntentPhase::MarkerLast,
                "{label}: the intent is never settled on an absence"
            );
        }
    }

    #[test]
    fn a_replay_refuses_an_intent_whose_foreign_capture_evidence_is_duplicated() {
        // The reviewer's erasure case: a journal with the recorded mismatch first
        // and an empty `foreign_captures` second. A reader that kept the last member
        // would drop the evidence and resume the intent, walking the put-back
        // foreign object as an ordinary entry. The journal is refused instead, and
        // nothing is unlinked.
        let prepared = prepared("replay-duplicate-foreign");
        plant_unit_contents(&prepared);
        let foreign = b"the racer's object".to_vec();
        let faults = Faults::new();
        faults.racing(
            &prepared.parent_path(),
            "payload.txt",
            2,
            Replacement::File(foreign.clone()),
        );
        let report = prepared.delete(&faults, &prepared.fixture.facts());
        assert!(report.is_pending(), "{report:?}");
        let leaf = report.quarantine.clone();
        assert_eq!(
            std::fs::read(leaf.join("payload.txt")).expect("the put-back object is there"),
            foreign
        );
        let pending = prepared.pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].foreign_captures.len(),
            1,
            "the mismatch is recorded before the put-back"
        );

        let path = state::intent_path(&prepared.fixture.state, &pending[0].intent_id)
            .expect("the intent path");
        let text = std::fs::read_to_string(&path).expect("the intent reads");
        std::fs::write(
            &path,
            crate::testkit::append_json_member(&text, "\"foreign_captures\":[]"),
        )
        .expect("the altered journal is written");

        let error = replay_with(
            &prepared.fixture.state,
            &Faults::new(),
            &prepared.fixture.facts(),
            &prepared.fixture.mounts(),
            &prepared.fixture.observer(),
            prepared.bounds(),
        )
        .expect_err("an ambiguous journal is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            std::fs::read(leaf.join("payload.txt"))
                .expect("the replay never unlinks the put-back foreign object"),
            foreign
        );
        // The journal itself is untouched, so an operator still has the evidence.
        let after = std::fs::read_to_string(&path).expect("the intent still reads");
        assert!(
            after.contains("foreign_captures"),
            "the refused read changed nothing"
        );
    }

    #[test]
    fn a_v1_intent_from_main_replays_as_a_barge_marked_directory_unit() {
        // `barge.intent/1` recorded a Barge root and nothing else. It stays
        // readable, maps to a Barge-marked directory unit, and a replay completes
        // it exactly as it would a v2 record.
        let prepared = prepared("delete-v1-replay");
        plant_unit_contents(&prepared);
        rename_to_quarantine(&prepared);
        let intent_id = format!("{:032x}", 1);
        let marker = prepared
            .unit
            .marker
            .as_ref()
            .expect("a Barge root carries its marker");
        let value = serde_json::json!({
            "schema": "barge.intent/1",
            "intent_id": intent_id,
            "root_id": prepared.unit.name,
            "volume_uuid": prepared.fixture.uuid.to_string(),
            "mount_hint": prepared.fixture.root,
            "root_dir": prepared.unit.root_dir,
            "folder": prepared.unit.folder,
            "root_name": prepared.unit.name,
            "quarantine_name": format!("{QUARANTINE_PREFIX}{intent_id}"),
            "device": prepared.unit.device.get(),
            "inode": prepared.unit.inode,
            "marker_digest": marker.digest,
            "plan_digest": PLAN,
            "phase": "renamed",
            "generation": 2,
            "recorded_ms": 1,
            "phase_ms": 2,
        });
        let path = crate::state::intent_path(&prepared.fixture.state, &intent_id)
            .expect("the intent path");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("the v1 record encodes"),
        )
        .expect("the v1 record is written");

        let read = crate::state::read_intent(&prepared.fixture.state, &intent_id)
            .expect("a v1 record reads");
        assert_eq!(read.unit.kind, UnitKind::Directory, "v1 is a directory");
        assert_eq!(read.unit.name, prepared.unit.name);
        assert_eq!(read.unit.parent, "");
        assert_eq!(
            read.unit.marker.as_ref().map(|marker| marker.family),
            Some(MarkerFamily::Barge),
            "v1 maps to a Barge-marked unit"
        );
        assert_eq!(
            read.unit.grant,
            UnitGrant::native_root(prepared.unit.name.clone()),
            "v1 maps to the native-root grant"
        );

        let replay = prepared.replay();
        assert_eq!(replay.resumed, 1, "{replay:?}");
        assert!(replay.is_settled(), "{replay:?}");
        assert!(!prepared.unit_path().exists(), "the v1 unit is deleted");
        assert!(prepared.pending_intents().is_empty());
    }

    #[test]
    fn an_unknown_intent_major_refuses_before_any_unlink() {
        let prepared = prepared("delete-unknown-major");
        plant_unit_contents(&prepared);
        publish(&prepared, &[]);
        let intent_id = format!("{:032x}", 1);
        let path = crate::state::intent_path(&prepared.fixture.state, &intent_id)
            .expect("the intent path");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("the intent reads"))
                .expect("the intent decodes");
        value["schema"] = serde_json::Value::String("barge.intent/3".to_owned());
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("the record encodes"),
        )
        .expect("the altered intent is written");

        let faults = Faults::new();
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let error = replay_with(
            &prepared.fixture.state,
            &faults,
            &prepared.fixture.facts(),
            &mounts,
            &observer,
            prepared.bounds(),
        )
        .expect_err("an unknown intent major refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(prepared.unit_path().exists(), "nothing is unlinked");
        assert!(
            prepared.unit_path().join("payload.txt").is_file(),
            "the unit's contents are untouched"
        );
    }
}
