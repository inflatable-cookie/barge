//! Whole-root deletion: the intent journal, the quarantine rename, Finder
//! tolerance, `ENOTEMPTY` retries and replay (stage 1 design, "Whole-root
//! deletion, Finder metadata and quarantine").
//!
//! This is the only code in Barge that may delete, and it is a library
//! capability: no CLI verb deletes and this slice wires no scheduled effect. A
//! deletion is whole-root only, of owned roots only, descriptor-relative and
//! bounded.
//!
//! Every phase is durable before the effect it authorises:
//!
//! 1. `intent` before the quarantine rename, recording the root id, marker
//!    digest, device, inode, volume UUID, plan digest and time;
//! 2. `renamed` before the first unlink;
//! 3. `marker_last` after the marker is unlinked last and before the quarantine
//!    leaf is removed;
//! 4. `deleted` once the leaf is gone, which settles the intent.
//!
//! A stop after the first effect leaves the intent pending and the quarantine
//! leaf in place; a later replay resolves that intent only from its recorded
//! device, inode and marker digest, and never infers that a deletion completed.
//! A symlink inside the quarantine stops the deletion and is never followed, and
//! an entry that is neither a plain file of this user on this device, a
//! directory, nor Finder metadata stops it too. No rule ever widens what is
//! deleted.
//!
//! Every entry is removed through one rule: **capture, verify, unlink**. The name
//! is exclusively renamed to `.barge-del-<random 128-bit nonce>` in the same
//! directory, the captured object's own facts are re-read under that private name
//! and compared with the observation the walk took of it, and only a match is
//! unlinked. A mismatch never unlinks: the object goes back under its original
//! name, or, when that name is taken, stays under the private name with both names
//! reported, and the deletion stops with the intent pending. That closes the
//! check-then-unlink-by-name race, in which a same-uid racer replaces each entry
//! after its check and the next check passes. A `.barge-del-*` name a stop left
//! behind is recorded with the observation its capture failed to match, and no run
//! and no replay ever unlinks one: it is unverifiable, so it keeps the intent
//! pending for an operator instead.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::area::{self, AreaDeclaration, AreaDocument, MarkerExpectation, ROOT_MARKER_FILE};
use crate::contract::{INTENT_ROWS, LISTING_NAME_ROWS};
use crate::custody::{self, CustodyBounds, CustodyEvidence, ObservedRoot, PendingIntent};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    ChildRemoval, DeviceId, DirHandle, DirOps, EntryFacts, EntryKind, MountRoot, ProcessObserver,
    RemoveKind, RenameOutcome, RetryEvidence, VolumeFacts, VolumeUuid,
};
use crate::registry;
use crate::state::{
    self, is_capture_name, IntentPhase, IntentRecord, IntentWrite, LeftoverRecord, LockSet,
    PutBackOutcome, StatePaths, CAPTURE_PREFIX, LEFTOVER_REASON_BYTES, QUARANTINE_PREFIX,
};

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

/// What one whole-root deletion did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteReport {
    /// The verified volume's UUID.
    pub volume_uuid: String,
    /// The root the deletion acted on.
    pub root_id: String,
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
    /// Whether the marker was unlinked last.
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
    /// The root the intent acts on.
    pub root_id: String,
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

/// Delete one owned root whole, on a registered volume.
///
/// Every refusal the registry, the volume, the declaration and custody can cause
/// happens before the first effect. Once the intent is published, a stop leaves
/// it pending and returns a report instead of pretending the root is gone; the
/// caller (S4's expiry or S7's `maintain`) reports that and exits `3`.
#[allow(clippy::too_many_arguments)]
pub fn delete_root_on_volume(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
    observed: &ObservedRoot,
    plan_digest: &str,
    bounds: DeleteBounds,
) -> Result<DeleteReport> {
    if !is_sha256(plan_digest) {
        return Err(Error::new(
            ErrorKind::Usage,
            "the plan digest is not a SHA-256 hex string".to_owned(),
        )
        .with_detail("plan_digest", plan_digest.into()));
    }
    if &observed.volume_uuid != volume_uuid {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "the observed root is on volume {}, not the requested {volume_uuid}",
                observed.volume_uuid
            ),
        ));
    }

    // Pre-flight, effect-free: an invalid registry, an unregistered or
    // undeclared volume, a mismatched or absent volume, a declaration that does
    // not contain its folder, and an observation that does not match the
    // configured resolution all refuse before a directory or lock exists.
    let record = area::registered_volume(state_paths, volume_uuid)?;
    record.declared_durability()?;
    let volume = area::prepare(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        declaration,
        stored,
    )?;
    let resolved = &volume.resolved;
    if observed.root_dir != resolved.root_dir || observed.folder != resolved.folder {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "the observed root is not in the folder the configuration resolves".to_owned(),
        )
        .with_detail("observed_root_dir", observed.root_dir.clone().into())
        .with_detail("configured_root_dir", resolved.root_dir.clone().into())
        .with_detail("observed_folder", observed.folder.clone().into())
        .with_detail("configured_folder", resolved.folder.clone().into()));
    }
    if !resolved.exists() {
        return Err(Error::new(
            ErrorKind::VolumeAbsent,
            format!("the area folder {} is not there", resolved.path.display()),
        )
        .with_detail("path", resolved.path.display().to_string().into()));
    }
    if !resolved.path.join(&observed.root_id).is_dir() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the root {} is not there, so there is nothing to delete",
                resolved.path.join(&observed.root_id).display()
            ),
        ));
    }

    state_paths.initialize()?;
    let mut locks = LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(volume_uuid)?;
    let _root_lock = locks.lock_root(&observed.root_id)?;

    // The registry is re-read under the locks before the intent is written, so a
    // record that changed while the walk ran is refused rather than trusted.
    let record = area::registered_volume(state_paths, volume_uuid)?;
    record.declared_durability()?;
    let Some((area_path, area_handle)) = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &observed.root_dir,
        &observed.folder,
    )?
    else {
        return Err(Error::new(
            ErrorKind::VolumeAbsent,
            "the area folder vanished before the intent was written".to_owned(),
        ));
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
        observed,
        &pending,
        bounds.custody,
    )
    .into_evidence()?;

    let intent_id = hex(&dirs.random_bytes(16)?);
    let quarantine = area_path.join(format!("{QUARANTINE_PREFIX}{intent_id}"));
    let now = state::now_ms();
    let mut intent = IntentRecord {
        schema: crate::contract::INTENT_SCHEMA.to_owned(),
        intent_id: intent_id.clone(),
        root_id: observed.root_id.clone(),
        volume_uuid: observed.volume_uuid.clone(),
        mount_hint: resolved.mount_point.clone(),
        root_dir: observed.root_dir.clone(),
        folder: observed.folder.clone(),
        root_name: observed.root_id.clone(),
        quarantine_name: format!("{QUARANTINE_PREFIX}{intent_id}"),
        device: observed.device.get(),
        inode: observed.inode,
        marker_digest: observed.marker_digest.clone(),
        plan_digest: plan_digest.to_owned(),
        phase: IntentPhase::Intent,
        generation: 1,
        recorded_ms: now,
        phase_ms: now,
        leftovers: Vec::new(),
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
        observed,
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

    // The quarantine rename is atomic and exclusive inside one directory: an
    // existing name refuses instead of being replaced, and a cross-device or
    // changed-inode result stops before anything is deleted.
    match dirs.rename_child(&area_handle, &observed.root_id, &intent.quarantine_name) {
        Ok(RenameOutcome::Renamed) => {}
        Ok(RenameOutcome::TargetExists) => {
            return Ok(pending_report(
                &intent,
                quarantine,
                Counts::default(),
                format!(
                    "the quarantine name `{}` is already taken, so the root was not renamed",
                    intent.quarantine_name
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
    match dirs.open_child_dir(&area_handle, &intent.quarantine_name) {
        Ok(leaf)
            if leaf.identity().device == observed.device
                && leaf.identity().inode == observed.inode => {}
        Ok(leaf) => {
            return Ok(pending_report(
                &intent,
                quarantine,
                Counts::default(),
                format!(
                    "the renamed leaf is device {} inode {}, not the recorded {} / {}",
                    leaf.identity().device,
                    leaf.identity().inode,
                    observed.device,
                    observed.inode
                ),
                Some(evidence),
            ))
        }
        Err(error) => {
            return Ok(pending_report(
                &intent,
                quarantine,
                Counts::default(),
                format!("the renamed leaf cannot be opened: {error}"),
                Some(evidence),
            ))
        }
    }
    intent = state::advance_intent(&write, state_paths, &intent, IntentPhase::Renamed)?;

    let mut report = finish_deletion(
        &write,
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        &intent,
        &area_path,
        &area_handle,
        bounds,
    );
    report.custody = Some(evidence);
    Ok(report)
}

/// Delete one quarantined root whole, from the phase the intent reached.
#[allow(clippy::too_many_arguments)]
fn finish_deletion(
    write: &IntentWrite,
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    _mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    intent: &IntentRecord,
    area_path: &Path,
    area_handle: &DirHandle,
    bounds: DeleteBounds,
) -> DeleteReport {
    let quarantine = area_path.join(&intent.quarantine_name);
    let leaf = match dirs.open_child_dir(area_handle, &intent.quarantine_name) {
        Ok(leaf) => leaf,
        Err(error) => {
            return pending_report(
                intent,
                quarantine,
                Counts::default(),
                format!("the quarantine leaf cannot be opened: {error}"),
                None,
            )
        }
    };
    if leaf.identity().device.get() != intent.device || leaf.identity().inode != intent.inode {
        return pending_report(
            intent,
            quarantine,
            Counts::default(),
            format!(
                "the quarantine leaf is device {} inode {}, not the recorded {} / {}",
                leaf.identity().device,
                leaf.identity().inode,
                intent.device,
                intent.inode
            ),
            None,
        );
    }

    let mut counts = Counts::default();
    let mut current = intent.clone();

    // The marker is the ownership proof; while it must still be there, its bytes
    // must still be the ones the intent recorded.
    if current.phase < IntentPhase::MarkerLast {
        match leaf_marker_digest(dirs, observer, intent, &leaf) {
            Ok(MarkerBytes::Digest(digest)) if digest == intent.marker_digest => {}
            Ok(_) => {
                return pending_report(
                    intent,
                    quarantine,
                    counts,
                    "the quarantine leaf's marker is not the recorded marker bytes".to_owned(),
                    None,
                )
            }
            Err(error) => {
                return pending_report(
                    intent,
                    quarantine,
                    counts,
                    format!("the quarantine leaf's marker is unreadable: {error}"),
                    None,
                )
            }
        }
    }

    let mut walk = match WalkState::new(
        dirs,
        facts,
        observer,
        intent.volume_uuid.clone(),
        DeviceId::new(intent.device),
        intent.mount_hint.clone(),
        bounds,
        &intent.leftovers,
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
        let classified = match walk.classify(&leaf, 0, Path::new("")) {
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
            return pending_with_leftovers(
                write,
                state_paths,
                intent,
                &walk.leftovers,
                quarantine,
                counts,
                reason,
            );
        }
        match walk.delete_contents(&leaf, 0, Path::new(""), true, &mut counts) {
            Ok(Walk::Stop(reason)) => {
                return pending_with_leftovers(
                    write,
                    state_paths,
                    intent,
                    &walk.leftovers,
                    quarantine,
                    counts,
                    reason,
                )
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
        // The marker is the ownership proof, and it is captured and verified like
        // every other entry before it is unlinked last.
        match walk.remove_marker_last(&leaf, &mut counts) {
            Ok(Walk::Clean) => {}
            Ok(Walk::Stop(reason)) => {
                return pending_with_leftovers(
                    write,
                    state_paths,
                    intent,
                    &walk.leftovers,
                    quarantine,
                    counts,
                    reason,
                )
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
        match state::advance_intent(write, state_paths, &current, IntentPhase::MarkerLast) {
            Ok(next) => current = next,
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

    // The marker is gone: only now may the quarantine leaf be removed, and only
    // through the descriptor that was verified as the recorded root.
    match walk.remove_directory(
        area_handle,
        &leaf,
        &current.quarantine_name,
        0,
        Path::new(""),
        &mut counts,
    ) {
        Ok(Removal::Gone) => {}
        Ok(Removal::Stopped(reason)) => {
            return pending_with_leftovers(
                write,
                state_paths,
                &current,
                &walk.leftovers,
                quarantine,
                counts,
                reason,
            )
        }
        Err(error) => {
            return pending_report(
                &current,
                quarantine,
                counts,
                format!("the quarantine leaf could not be removed: {error}"),
                None,
            )
        }
    }
    counts.directories = counts.directories.saturating_add(1);
    match state::advance_intent(write, state_paths, &current, IntentPhase::Deleted) {
        Ok(settled) => DeleteReport {
            volume_uuid: settled.volume_uuid.to_string(),
            root_id: settled.root_id.clone(),
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

/// What one quarantined leaf's marker is: absent, or the digest of the bytes
/// that are there. A marker that is there but unreadable is an error, never
/// "absent", so a deletion never mistakes an unknown marker for a removed one.
enum MarkerBytes {
    Absent,
    Digest(String),
}

/// Read one quarantined leaf's marker digest. The marker records the root's
/// creation path, which the rename did not change, so the expectation is only
/// what lets the bytes be read; the caller compares the digest.
fn leaf_marker_digest(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    intent: &IntentRecord,
    leaf: &DirHandle,
) -> Result<MarkerBytes> {
    let expectation = MarkerExpectation {
        volume_uuid: &intent.volume_uuid,
        resolved_path: Path::new(""),
        effective_uid: observer.effective_uid()?,
        now_ms: state::now_ms(),
    };
    let (marker, digest) = area::read_marker_with_digest(dirs, leaf, &expectation);
    match marker {
        crate::area::MarkerState::Absent => Ok(MarkerBytes::Absent),
        _ => match digest {
            Some(digest) => Ok(MarkerBytes::Digest(digest)),
            None => Err(Error::new(
                ErrorKind::UnknownState,
                "the quarantine leaf's marker bytes could not be read".to_owned(),
            )),
        },
    }
}

/// Report a stop, publishing the unverified captures the walk could not put back.
///
/// The record is durable before the report, so a later run and a replay know which
/// private names hold objects whose capture never verified. A record that cannot be
/// written does not weaken the stop: the intent stays pending either way, and an
/// unrecorded private name is still unverifiable and still never unlinked.
fn pending_with_leftovers(
    write: &IntentWrite,
    state_paths: &StatePaths,
    intent: &IntentRecord,
    leftovers: &[LeftoverRecord],
    quarantine: PathBuf,
    counts: Counts,
    reason: String,
) -> DeleteReport {
    let mut recorded = intent.clone();
    if !leftovers.is_empty() {
        match state::record_leftovers(write, state_paths, intent, leftovers) {
            Ok(next) => recorded = next,
            Err(error) => {
                return pending_report(
                    &recorded,
                    quarantine,
                    counts,
                    format!("{reason}; the unverified capture could not be recorded: {error}"),
                    None,
                )
            }
        }
    }
    pending_report(&recorded, quarantine, counts, reason, None)
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
        root_id: intent.root_id.clone(),
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
    /// The unverified captures the intent already records, so a stop can name the
    /// object's original name and why its capture did not verify.
    recorded: Vec<LeftoverRecord>,
    /// The unverified captures this run could not put back, which the caller
    /// publishes into the intent before it reports the stop.
    leftovers: Vec<LeftoverRecord>,
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
        recorded: &[LeftoverRecord],
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
            recorded: recorded.to_vec(),
            leftovers: Vec::new(),
        })
    }

    /// The stop for a private name this run did not capture.
    ///
    /// A leftover is unverifiable: nothing recorded the identity of the object
    /// under the private name for the walk to check it against, so no run and no
    /// replay may unlink it. A recorded capture additionally names what it was
    /// and why its verification failed.
    fn leftover_stop(&self, private: &str) -> String {
        match self
            .recorded
            .iter()
            .find(|leftover| leftover.private == private)
        {
            Some(record) => format!(
                "`{private}` holds the unverified capture of `{}`, whose put back {}: {}; it is never unlinked, so the intent stays pending",
                record.original,
                match record.put_back {
                    PutBackOutcome::Renamed => "put it back under its own name",
                    PutBackOutcome::Taken => "was refused because the name was taken",
                    PutBackOutcome::Failed => "failed",
                },
                record.reason
            ),
            None => format!(
                "`{private}` is a private capture name this deletion did not create, so its object is unverifiable and nothing is unlinked"
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
            if depth == 0 && entry.name == ROOT_MARKER_FILE {
                continue;
            }
            let Some(facts) = self.dirs.child_facts(handle, &entry.name)? else {
                // It vanished between the listing and the read.
                continue;
            };
            if let Walk::Stop(reason) = self.classify_entry(&entry.name, &facts) {
                return Ok(Walk::Stop(reason));
            }
            if is_capture_name(&entry.name) {
                // A private name is never an ordinary entry: it holds an object
                // whose capture this walk cannot verify, so it stops the deletion
                // and is never recorded as something the delete pass may remove.
                return Ok(Walk::Stop(self.leftover_stop(&entry.name)));
            }
            let path = relative.join(&entry.name);
            self.expected.insert(path.clone());
            if facts.kind == EntryKind::Directory {
                let child = self.dirs.open_child_dir(handle, &entry.name)?;
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
    /// on this device, inside the size bound. The
    /// parent binding is re-read through the descriptor the capture went through,
    /// so a volume that changed under it stops the deletion before any unlink.
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
            return Ok(Verify::Verified);
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
    /// unlink only on a match.
    ///
    /// This is the whole deletion rule: the object that is unlinked is the object
    /// that was verified after it was captured, so a same-uid racer that replaces
    /// the name between the check and the capture cannot have its own object
    /// removed. A mismatch never unlinks; the object is put back under its
    /// original name, or, when that name is taken, left under the private name
    /// with both names reported, and the deletion stops.
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
        if let Verify::Mismatch { reason, captured } =
            self.verify_capture(handle, &private, observed, allowance)?
        {
            // Never unlink. A put-back that itself fails still reports both the
            // original and the private name, because the operator has to decide
            // what happens to the object left under the private one.
            let (put_back, stopped) = match self.put_back(handle, name, &private, &reason) {
                Ok((put_back, stopped)) => (put_back, stopped),
                Err(error) => (
                    PutBack::Failed,
                    format!(
                        "`{name}` changed after its check and could not be put back from `{private}`: {error}"
                    ),
                ),
            };
            // Record the mismatch whatever the put-back answered. A successful
            // put-back leaves the captured object under its original name, where
            // an ordinary walk would find it again and unlink it; the record is
            // what tells a later run and a replay that Barge never observed it.
            if let Some(captured) = captured {
                self.leftovers.push(LeftoverRecord {
                    private,
                    original: name.to_owned(),
                    device: observed.device.get(),
                    inode: observed.inode,
                    kind: observed.kind.as_str().to_owned(),
                    captured_device: captured.device.get(),
                    captured_inode: captured.inode,
                    captured_kind: captured.kind.as_str().to_owned(),
                    put_back: match put_back {
                        PutBack::Renamed => PutBackOutcome::Renamed,
                        PutBack::Taken => PutBackOutcome::Taken,
                        PutBack::Failed => PutBackOutcome::Failed,
                    },
                    reason: bounded_reason(&reason),
                });
            }
            return Ok(Walk::Stop(stopped));
        }
        match observed.kind {
            EntryKind::File => {
                match self.dirs.remove_child(handle, &private, RemoveKind::File)? {
                    ChildRemoval::Removed => counts.entries = counts.entries.saturating_add(1),
                    ChildRemoval::Absent => {
                        return Ok(Walk::Stop(format!(
                            "the verified `{name}` vanished from `{private}` before it was unlinked"
                        )))
                    }
                    ChildRemoval::NotEmpty => {
                        return Ok(Walk::Stop(format!(
                            "`{private}` reported not-empty as a file"
                        )))
                    }
                }
                Ok(Walk::Clean)
            }
            EntryKind::Directory => {
                let child = match self.dirs.open_child_dir(handle, &private) {
                    Ok(child) => child,
                    Err(error) => {
                        return Ok(Walk::Stop(format!(
                            "the verified `{name}` could not be opened under `{private}`: {error}"
                        )))
                    }
                };
                let opened = child.identity();
                if opened.device != observed.device || opened.inode != observed.inode {
                    return Ok(Walk::Stop(format!(
                        "`{name}` under `{private}` opened as device {} inode {}, not the verified {} / {}, so nothing is removed",
                        opened.device, opened.inode, observed.device, observed.inode
                    )));
                }
                if let Walk::Stop(reason) =
                    self.delete_contents(&child, depth + 1, relative, false, counts)?
                {
                    return Ok(Walk::Stop(reason));
                }
                // The private name can only be replaced once the directory is
                // empty; the verified inode is what is removed, and nothing else.
                let still_verified =
                    self.dirs
                        .child_facts(handle, &private)?
                        .is_some_and(|facts| {
                            facts.kind == EntryKind::Directory
                                && facts.device == observed.device
                                && facts.inode == observed.inode
                        });
                if !still_verified {
                    return Ok(Walk::Stop(format!(
                        "`{private}` is no longer the verified directory, so it is not removed"
                    )));
                }
                match self.remove_directory(
                    handle,
                    &child,
                    &private,
                    depth + 1,
                    relative,
                    counts,
                )? {
                    Removal::Gone => {
                        counts.directories = counts.directories.saturating_add(1);
                        Ok(Walk::Clean)
                    }
                    Removal::Stopped(reason) => Ok(Walk::Stop(reason)),
                }
            }
            EntryKind::Symlink | EntryKind::Other => Ok(Walk::Stop(format!(
                "`{name}` is a {}, which the deletion never captures",
                observed.kind.as_str()
            ))),
        }
    }

    /// Put a captured object back under its original name after a failed
    /// verification, and report how the deletion stops.
    ///
    /// Nothing is unlinked on this path. When the original name is taken the
    /// object stays under its private name and both names are reported, because
    /// an operator has to decide which of the two should exist; that is the case
    /// the caller records as an unverified leftover.
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
        let Some(observed) = self.dirs.child_facts(leaf, ROOT_MARKER_FILE)? else {
            return Ok(Walk::Stop(
                "the marker vanished before it was unlinked last".to_owned(),
            ));
        };
        if let Walk::Stop(reason) = self.classify_entry(ROOT_MARKER_FILE, &observed) {
            return Ok(Walk::Stop(reason));
        }
        if observed.kind != EntryKind::File {
            return Ok(Walk::Stop(format!(
                "the marker is a {}, not a regular file",
                observed.kind.as_str()
            )));
        }
        match self.capture_and_remove(
            leaf,
            ROOT_MARKER_FILE,
            &observed,
            Allowance::Observed,
            0,
            Path::new(""),
            counts,
        )? {
            Walk::Clean => {
                counts.marker_removed = true;
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
    /// class), and a private name a stopped deletion left behind (re-verified
    /// against its own observation); a new entry that is none of those stops the
    /// deletion rather than widening it.
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
            if skip_marker && entry.name == ROOT_MARKER_FILE {
                continue;
            }
            // This read is the observation the capture is verified against.
            let Some(facts) = self.dirs.child_facts(handle, &entry.name)? else {
                continue;
            };
            if let Walk::Stop(reason) = self.classify_entry(&entry.name, &facts) {
                return Ok(Walk::Stop(reason));
            }
            if is_capture_name(&entry.name) {
                // Nothing recorded the identity of the object under this private
                // name for the walk to verify, so it is never captured again and
                // never unlinked: it keeps the intent pending.
                return Ok(Walk::Stop(self.leftover_stop(&entry.name)));
            }
            let path = relative.join(&entry.name);
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

    /// Remove one verified directory by its captured name, retrying `ENOTEMPTY`
    /// at most the declared number of times because Finder can recreate
    /// `.DS_Store` mid-delete. Each retry re-deletes only what the walk already
    /// recognises, through the descriptor of the directory that was verified as
    /// the captured one; it never re-opens the name to widen the walk.
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
                ChildRemoval::Removed | ChildRemoval::Absent => return Ok(Removal::Gone),
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

/// Why one entry may be removed at all.
///
/// A private capture name is never on this list: nothing recorded the identity of
/// the object under it for the walk to verify, so it is unverifiable and stops the
/// deletion instead of being captured again.
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
    /// class.
    Verified,
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
    if reason.len() <= LEFTOVER_REASON_BYTES {
        return reason.to_owned();
    }
    let mut end = LEFTOVER_REASON_BYTES;
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
/// A pending intent is resolved only from its recorded device, inode and marker
/// digest: the quarantine leaf exists with them, or both the leaf and the marker
/// are positively absent and the `marker_last` phase was recorded. Anything else
/// stays pending, is reported, and blocks reuse of that leaf.
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
                root_id: intent.root_id.clone(),
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
                    root_id: intent.root_id.clone(),
                    phase: intent.phase,
                    reason,
                }),
            },
            Err(error) => report.pending.push(PendingRow {
                intent_id: intent.intent_id.clone(),
                root_id: intent.root_id.clone(),
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
    /// The root was still in place, so the intent was rolled back.
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
        // The area folder is positively gone, so the leaf and the marker are
        // too. Only a completed `marker_last` phase proves that.
        if intent.phase == IntentPhase::MarkerLast {
            settle(state_paths, intent)?;
            return Ok(ReplayOutcome::Settled);
        }
        return Ok(ReplayOutcome::Pending(
            "the area folder is gone but the marker-last phase was not recorded".to_owned(),
        ));
    };

    let mut locks = LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(&intent.volume_uuid)?;
    let _root_lock = locks.lock_root(&intent.root_id)?;
    let write = locks.begin_intent_write()?;

    let leaf_facts = dirs.child_facts(&area_handle, &intent.quarantine_name)?;
    let leaf_matches = leaf_facts.as_ref().is_some_and(|facts| {
        facts.kind == EntryKind::Directory
            && facts.device.get() == intent.device
            && facts.inode == intent.inode
    });
    // One handle to the leaf serves the marker proof and the leaf removal, so the
    // retry that empties it deletes through the descriptor that was verified.
    let leaf_handle = if leaf_matches {
        match dirs.open_child_dir(&area_handle, &intent.quarantine_name) {
            Ok(leaf) => Some(leaf),
            Err(error) => {
                return Ok(ReplayOutcome::Pending(format!(
                    "the quarantine leaf cannot be opened: {error}"
                )))
            }
        }
    } else {
        None
    };
    let marker_digest = match &leaf_handle {
        Some(leaf) => Some(leaf_marker_digest(dirs, observer, intent, leaf)?),
        None => None,
    };

    match intent.phase {
        IntentPhase::Intent => {
            // The rename may not have happened: if the root is still in place
            // with its recorded identity, the intent is rolled back.
            if root_still_in_place(dirs, observer, intent, &area_handle)? {
                state::remove_intent(state_paths, &intent.intent_id)?;
                return Ok(ReplayOutcome::RolledBack);
            }
            if leaf_matches && marker_matches(&marker_digest, intent) {
                if let Some(reason) = recorded_mismatch(&intent.leftovers) {
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
                    mounts,
                    observer,
                    &renamed,
                    &area_path,
                    &area_handle,
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
                "neither the root nor the quarantine leaf matches the recorded identity".to_owned(),
            ))
        }
        IntentPhase::Renamed => {
            if let Some(reason) = recorded_mismatch(&intent.leftovers) {
                return Ok(ReplayOutcome::Pending(reason));
            }
            if leaf_matches && marker_matches(&marker_digest, intent) {
                let report = finish_deletion(
                    &write,
                    state_paths,
                    dirs,
                    facts,
                    mounts,
                    observer,
                    intent,
                    &area_path,
                    &area_handle,
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
                "the quarantine leaf's recorded identity and marker bytes do not match".to_owned(),
            ))
        }
        IntentPhase::MarkerLast => {
            if let Some(reason) = recorded_mismatch(&intent.leftovers) {
                return Ok(ReplayOutcome::Pending(reason));
            }
            if leaf_facts.is_none() {
                // Both the leaf and the marker are positively absent, and the
                // marker-last phase was recorded: that is the proof.
                state::advance_intent(&write, state_paths, intent, IntentPhase::Deleted)?;
                return Ok(ReplayOutcome::Settled);
            }
            let Some(leaf) = leaf_handle.as_ref() else {
                return Ok(ReplayOutcome::Pending(
                    "the leaf and marker do not prove completion".to_owned(),
                ));
            };
            if !matches!(marker_digest, Some(MarkerBytes::Absent)) {
                return Ok(ReplayOutcome::Pending(
                    "the leaf and marker do not prove completion".to_owned(),
                ));
            }
            let mut walk = WalkState::new(
                dirs,
                facts,
                observer,
                intent.volume_uuid.clone(),
                DeviceId::new(intent.device),
                intent.mount_hint.clone(),
                bounds,
                &intent.leftovers,
            )?;
            walk.verify_volume()?;
            let mut counts = Counts::default();
            match walk.remove_directory(
                &area_handle,
                leaf,
                &intent.quarantine_name,
                0,
                Path::new(""),
                &mut counts,
            )? {
                Removal::Gone => {
                    state::advance_intent(&write, state_paths, intent, IntentPhase::Deleted)?;
                    Ok(ReplayOutcome::Settled)
                }
                Removal::Stopped(reason) => {
                    // The retry may have captured an object it could not put back;
                    // record it before the pending answer is reported.
                    if walk.leftovers.is_empty() {
                        return Ok(ReplayOutcome::Pending(reason));
                    }
                    match state::record_leftovers(&write, state_paths, intent, &walk.leftovers) {
                        Ok(_) => Ok(ReplayOutcome::Pending(reason)),
                        Err(error) => Ok(ReplayOutcome::Pending(format!(
                            "{reason}; the unverified capture could not be recorded: {error}"
                        ))),
                    }
                }
            }
        }
        IntentPhase::Deleted => Ok(ReplayOutcome::Settled),
    }
}

/// Whether the root is still in place with the recorded identity and marker
/// bytes, which is what makes a rollback safe.
fn root_still_in_place(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    intent: &IntentRecord,
    area_handle: &DirHandle,
) -> Result<bool> {
    let Some(facts) = dirs.child_facts(area_handle, &intent.root_name)? else {
        return Ok(false);
    };
    if facts.kind != EntryKind::Directory
        || facts.device.get() != intent.device
        || facts.inode != intent.inode
    {
        return Ok(false);
    }
    let leaf = dirs.open_child_dir(area_handle, &intent.root_name)?;
    Ok(marker_matches(
        &Some(leaf_marker_digest(dirs, observer, intent, &leaf)?),
        intent,
    ))
}

/// Whether the quarantine leaf's marker is exactly the bytes the intent
/// recorded.
fn marker_matches(digest: &Option<MarkerBytes>, intent: &IntentRecord) -> bool {
    matches!(digest, Some(MarkerBytes::Digest(bytes)) if bytes == &intent.marker_digest)
}

/// The replay stop naming every capture whose verification failed.
///
/// A recorded mismatch proves an object Barge never observed is somewhere in the
/// leaf: under its private name, or back under its original name when the put-back
/// succeeded. No replay resumes such an intent, because resuming would walk that
/// object as an ordinary entry of the root and unlink it.
fn recorded_mismatch(leftovers: &[LeftoverRecord]) -> Option<String> {
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

/// Settle a `marker_last` intent whose area folder is gone.
fn settle(state_paths: &StatePaths, intent: &IntentRecord) -> Result<()> {
    let mut locks = LockSet::new(state_paths.clone());
    let _volume_lock = locks.lock_volume(&intent.volume_uuid)?;
    let _root_lock = locks.lock_root(&intent.root_id)?;
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

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        delete_root_on_volume, replay_pending, DeleteBounds, DeleteOutcome, ReplayReport,
        CAPTURE_PREFIX,
    };
    use crate::area::{AreaDeclaration, RootKind, ROOT_MARKER_FILE};
    use crate::custody::ObservedRoot;
    use crate::error::ErrorKind;
    use crate::os::{
        ChildRemoval, DescriptorScan, DescriptorScanRequest, DeviceId, DirOps, EntryKind,
        ProcessObserver, RemoveKind, RenameOutcome, RetriedUnreadable, RetryEvidence,
        RetryResolution,
    };
    use crate::state::{self, IntentPhase, IntentRecord, LockSet, QUARANTINE_PREFIX};
    use crate::testkit::{FakeObserver, Faults, Replacement, VolumeFixture};

    const PLAN: &str = "abababababababababababababababababababababababababababababababab";

    struct Prepared {
        fixture: VolumeFixture,
        declaration: AreaDeclaration,
        observed: ObservedRoot,
    }

    fn prepared(label: &str) -> Prepared {
        let fixture = VolumeFixture::new(label);
        fixture.register();
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) =
            fixture.create_root(&fixture.record, &declaration, RootKind::AreaRoot);
        let observed = ObservedRoot::from_owned(&root, &resolved.root_dir, &resolved.folder);
        Prepared {
            fixture,
            declaration,
            observed,
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
            delete_root_on_volume(
                &self.fixture.state,
                dirs,
                facts,
                &mounts,
                observer,
                &self.fixture.uuid,
                &self.declaration,
                None,
                &self.observed,
                PLAN,
                self.bounds(),
            )
            .expect("the deletion reports rather than fails")
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
            self.observed
                .path
                .parent()
                .expect("the area folder")
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
            root_id: prepared.observed.root_id.clone(),
            volume_uuid: prepared.observed.volume_uuid.clone(),
            mount_hint: prepared.fixture.root.clone(),
            root_dir: prepared.observed.root_dir.clone(),
            folder: prepared.observed.folder.clone(),
            root_name: prepared.observed.root_id.clone(),
            quarantine_name: format!("{QUARANTINE_PREFIX}{intent_id}"),
            device: prepared.observed.device.get(),
            inode: prepared.observed.inode,
            marker_digest: prepared.observed.marker_digest.clone(),
            plan_digest: PLAN.to_owned(),
            phase: IntentPhase::Intent,
            generation: 1,
            recorded_ms: now,
            phase_ms: now,
            leftovers: Vec::new(),
        };
        let mut locks = LockSet::new(prepared.fixture.state.clone());
        let _volume = locks
            .lock_volume(&prepared.fixture.uuid)
            .expect("the volume lock");
        let _root = locks
            .lock_root(&prepared.observed.root_id)
            .expect("the root lock");
        let write = locks.begin_intent_write().expect("the intent step");
        state::publish_intent(&write, &prepared.fixture.state, &intent)
            .expect("the intent publishes");
        for phase in phases {
            intent = state::advance_intent(&write, &prepared.fixture.state, &intent, *phase)
                .expect("the phase advances");
        }
        intent
    }

    fn rename_to_quarantine(prepared: &Prepared) {
        std::fs::rename(
            &prepared.observed.path,
            prepared
                .observed
                .path
                .parent()
                .expect("the area folder")
                .join(format!("{QUARANTINE_PREFIX}{:032x}", 1)),
        )
        .expect("the root is quarantined");
    }

    #[test]
    fn an_owned_root_is_deleted_whole_and_the_marker_is_unlinked_last() {
        let prepared = prepared("delete-clean");
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");

        let faults = Faults::new();
        faults.failing_remove_prefix(QUARANTINE_PREFIX, 3);
        let facts = prepared.fixture.facts();
        let report = prepared.delete(&faults, &facts);
        assert!(report.is_pending(), "{report:?}");
        assert_eq!(report.phase, IntentPhase::MarkerLast, "{report:?}");
        assert!(report.marker_removed, "the marker was unlinked last");
        assert!(report.quarantine.is_dir(), "the leaf is kept");
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
    fn replay_settles_a_marker_last_intent_whose_leaf_and_marker_are_absent() {
        let prepared = prepared("replay-both-absent");
        let root = prepared.observed.path.clone();
        VolumeFixture::payload(&root, "payload.txt", b"bytes");
        publish(&prepared, &[]);
        rename_to_quarantine(&prepared);
        std::fs::remove_dir_all(prepared.quarantine()).expect("the leaf is removed");
        publish(&prepared, &[IntentPhase::Renamed, IntentPhase::MarkerLast]);

        let report = prepared.replay();
        assert_eq!(report.settled, 1, "{report:?}");
        assert!(report.is_settled());
        assert_eq!(prepared.pending_intents().len(), 0);
    }

    #[test]
    fn replay_never_infers_completion_from_a_missing_marker() {
        let prepared = prepared("replay-missing-marker");
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let planted = report.quarantine.join(".DS_Store");
        assert!(
            std::fs::symlink_metadata(&planted)
                .expect("the planted entry is still there")
                .file_type()
                .is_dir(),
            "the Finder-shaped directory is never recursively deleted"
        );
        assert!(report.quarantine.is_dir(), "the leaf is kept");
    }

    #[test]
    fn a_settled_intent_is_not_pending_work() {
        let prepared = prepared("replay-settled");
        let root = prepared.observed.path.clone();
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
        let error = delete_root_on_volume(
            &prepared.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &prepared.fixture.uuid,
            &prepared.declaration,
            None,
            &prepared.observed,
            "not-a-digest",
            prepared.bounds(),
        )
        .expect_err("a bad plan digest refuses");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(prepared.observed.path.is_dir(), "nothing happened");
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
        std::fs::remove_dir_all(&prepared.observed.path).expect("the root is removed");
        let faults = Faults::new();
        let facts = prepared.fixture.facts();
        let mounts = prepared.fixture.mounts();
        let observer = prepared.fixture.observer();
        let error = delete_root_on_volume(
            &prepared.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &prepared.fixture.uuid,
            &prepared.declaration,
            None,
            &prepared.observed,
            PLAN,
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
            .join(&prepared.observed.root_id)
            .join("writer.json");
        std::fs::create_dir_all(path.parent().expect("the hold parent")).expect("the holds dir");
        std::fs::write(
            &path,
            VolumeFixture::hold_record(
                &prepared.observed.root_id,
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
        let error = delete_root_on_volume(
            &prepared.fixture.state,
            &faults,
            &facts,
            &mounts,
            &observer,
            &prepared.fixture.uuid,
            &prepared.declaration,
            None,
            &prepared.observed,
            PLAN,
            prepared.bounds(),
        )
        .expect_err("a live holder refuses");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
        assert!(prepared.observed.path.is_dir(), "the root is untouched");
        assert!(
            prepared.pending_intents().is_empty(),
            "no intent is written"
        );
    }

    #[test]
    fn a_finder_file_that_reappears_during_the_delete_is_tolerated() {
        let prepared = prepared("delete-finder-reappears");
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let faults = Faults::new();
        let foreign = b"the racer's object";
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        faults.occupying_after_capture(&area, "payload.txt");
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
        let recorded = &pending[0].leftovers;
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
        assert_eq!(recorded[0].captured_kind, "file");
        assert_ne!(
            recorded[0].captured_inode, recorded[0].inode,
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        assert!(private.is_file(), "the leftover is never unlinked");
        assert!(leaf.is_dir(), "the intent stays pending with its leaf");
        assert_eq!(prepared.pending_intents().len(), 1);
    }

    #[test]
    fn replay_after_a_successful_put_back_keeps_the_foreign_object_and_the_intent() {
        // The review's second replay gap: a put-back that succeeds leaves the
        // captured object back under its original name, where an ordinary walk
        // finds it again. The mismatch is durable evidence, so a replay must not
        // resume and unlink the object Barge never observed.
        let prepared = prepared("replay-after-successful-put-back");
        let root = prepared.observed.path.clone();
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
        let recorded = &pending[0].leftovers;
        assert_eq!(recorded.len(), 1, "the mismatch is recorded: {recorded:?}");
        assert_eq!(recorded[0].original, "payload.txt");
        assert_eq!(recorded[0].put_back, state::PutBackOutcome::Renamed);
        assert_ne!(
            recorded[0].captured_inode, recorded[0].inode,
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
        let root = prepared.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
        assert_eq!(pending[0].leftovers.len(), 1, "the mismatch is recorded");
    }

    #[test]
    fn replay_after_a_put_back_collision_keeps_the_foreign_object_and_the_intent() {
        // The review's replay defect: a collision left a foreign object under a
        // private name. A replay must not reclassify it as an ordinary owned entry
        // and unlink it; it must keep it and stay pending.
        let prepared = prepared("replay-after-collision");
        let root = prepared.observed.path.clone();
        let area = root.parent().expect("the area folder").to_path_buf();
        VolumeFixture::payload(&root, "payload.txt", b"mine");
        let foreign = b"the racer's object";
        let faults = Faults::new();
        faults.racing(&area, "payload.txt", 2, Replacement::File(foreign.to_vec()));
        faults.occupying_after_capture(&area, "payload.txt");
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
        assert_eq!(pending[0].leftovers.len(), 1);
    }

    #[test]
    fn replay_keeps_a_leftover_private_name_that_does_not_verify() {
        let prepared = prepared("replay-leftover-symlink");
        let root = prepared.observed.path.clone();
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
        assert!(std::fs::symlink_metadata(&private)
            .expect("the leftover is kept")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read(&victim).expect("the link target is untouched"),
            b"never removed"
        );
        assert!(leaf.is_dir(), "the intent stays pending with its leaf");
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
            let old_root = old.observed.path.clone();
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
            let new_root = new.observed.path.clone();
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
        let root = prepared.observed.path.clone();
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
            let root = prepared.observed.path.clone();
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
}
