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
    RemoveKind, VolumeFacts, VolumeUuid,
};
use crate::registry;
use crate::state::{
    self, IntentPhase, IntentRecord, IntentWrite, LockSet, StatePaths, QUARANTINE_PREFIX,
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
    /// How many entries were removed.
    pub entries_removed: usize,
    /// How many directories were removed, the leaf included.
    pub directories_removed: usize,
    /// Whether the marker was unlinked last.
    pub marker_removed: bool,
    /// The custody evidence that authorised the first effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custody: Option<CustodyEvidence>,
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
        return Ok(pending_report(
            &intent,
            quarantine,
            Counts::default(),
            format!(
                "custody is not proven after the intent was written: {}",
                after_publish.reason().unwrap_or("the verdict is not safe")
            ),
            Some(evidence),
        ));
    }

    // The quarantine rename is atomic inside one directory; a cross-device or
    // changed-inode result stops before anything is deleted.
    if let Err(error) = dirs.rename_child(&area_handle, &observed.root_id, &intent.quarantine_name)
    {
        return Ok(pending_report(
            &intent,
            quarantine,
            Counts::default(),
            format!("the quarantine rename did not happen: {error}"),
            Some(evidence),
        ));
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
            return pending_report(intent, quarantine, counts, reason, None);
        }
        match walk.delete_contents(&leaf, 0, Path::new(""), true, &mut counts) {
            Ok(Walk::Stop(reason)) => {
                return pending_report(intent, quarantine, counts, reason, None)
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
        match dirs.remove_child(&leaf, ROOT_MARKER_FILE, RemoveKind::File) {
            Ok(ChildRemoval::Removed) => counts.marker_removed = true,
            Ok(ChildRemoval::Absent) => {
                return pending_report(
                    intent,
                    quarantine,
                    counts,
                    "the marker vanished before it was unlinked last".to_owned(),
                    None,
                )
            }
            Ok(ChildRemoval::NotEmpty) => {
                return pending_report(
                    intent,
                    quarantine,
                    counts,
                    "the marker could not be removed".to_owned(),
                    None,
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

    // The marker is gone: only now may the quarantine leaf be removed.
    match walk.remove_directory(
        area_handle,
        &current.quarantine_name,
        0,
        Path::new(""),
        &mut counts,
    ) {
        Ok(Removal::Gone) => {}
        Ok(Removal::Stopped(reason)) => {
            return pending_report(&current, quarantine, counts, reason, None)
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
}

impl<'a> WalkState<'a> {
    fn new(
        dirs: &'a dyn DirOps,
        facts: &'a dyn VolumeFacts,
        observer: &'a dyn ProcessObserver,
        volume_uuid: VolumeUuid,
        device: DeviceId,
        mount: PathBuf,
        bounds: DeleteBounds,
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
        })
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

    /// Whether one entry may be removed: it was classified, or it is Finder
    /// metadata that appeared again. Nothing else is ever removed.
    ///
    /// Finder tolerance is exactly Oxide's two forms *and* a regular file: a
    /// directory, a link or anything else that happens to be named `.DS_Store`
    /// or `._name` is not Finder metadata, stops the delete and keeps the root.
    /// The owner and device are already checked by the classification.
    fn may_remove(&self, relative: &Path, name: &str, facts: &EntryFacts) -> bool {
        self.expected.contains(relative)
            || (facts.kind == EntryKind::File && is_finder_metadata(name))
    }

    /// Delete every entry of one directory except, at the top level, the marker.
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
            let Some(facts) = self.dirs.child_facts(handle, &entry.name)? else {
                continue;
            };
            if let Walk::Stop(reason) = self.classify_entry(&entry.name, &facts) {
                return Ok(Walk::Stop(reason));
            }
            let path = relative.join(&entry.name);
            if !self.may_remove(&path, &entry.name, &facts) {
                return Ok(Walk::Stop(format!(
                    "`{}` appeared after the classification, so the delete stops",
                    path.display()
                )));
            }
            if facts.kind == EntryKind::Directory {
                let child = self.dirs.open_child_dir(handle, &entry.name)?;
                if let Walk::Stop(reason) =
                    self.delete_contents(&child, depth + 1, &path, false, counts)?
                {
                    return Ok(Walk::Stop(reason));
                }
                match self.remove_directory(handle, &entry.name, depth + 1, &path, counts)? {
                    Removal::Gone => {
                        counts.directories = counts.directories.saturating_add(1);
                    }
                    Removal::Stopped(reason) => return Ok(Walk::Stop(reason)),
                }
            } else {
                match self
                    .dirs
                    .remove_child(handle, &entry.name, RemoveKind::File)?
                {
                    ChildRemoval::Removed => {
                        counts.entries = counts.entries.saturating_add(1);
                    }
                    ChildRemoval::Absent => {}
                    ChildRemoval::NotEmpty => {
                        return Ok(Walk::Stop(format!("`{}` could not be removed", entry.name)))
                    }
                }
            }
        }
        Ok(Walk::Clean)
    }

    /// Remove one directory, retrying `ENOTEMPTY` at most the declared number of
    /// times because Finder can recreate `.DS_Store` mid-delete. The retry
    /// re-deletes only what the walk already recognises; it never widens it.
    fn remove_directory(
        &mut self,
        parent: &DirHandle,
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
                    let child = self.dirs.open_child_dir(parent, name)?;
                    if let Walk::Stop(reason) =
                        self.delete_contents(&child, depth, relative, false, counts)?
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
    let marker_digest = if leaf_matches {
        match dirs.open_child_dir(&area_handle, &intent.quarantine_name) {
            Ok(leaf) => Some(leaf_marker_digest(dirs, observer, intent, &leaf)?),
            Err(error) => {
                return Ok(ReplayOutcome::Pending(format!(
                    "the quarantine leaf cannot be opened: {error}"
                )))
            }
        }
    } else {
        None
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
            if leaf_facts.is_none() {
                // Both the leaf and the marker are positively absent, and the
                // marker-last phase was recorded: that is the proof.
                state::advance_intent(&write, state_paths, intent, IntentPhase::Deleted)?;
                return Ok(ReplayOutcome::Settled);
            }
            if leaf_matches && matches!(marker_digest, Some(MarkerBytes::Absent)) {
                let mut walk = WalkState::new(
                    dirs,
                    facts,
                    observer,
                    intent.volume_uuid.clone(),
                    DeviceId::new(intent.device),
                    intent.mount_hint.clone(),
                    bounds,
                )?;
                walk.verify_volume()?;
                let mut counts = Counts::default();
                match walk.remove_directory(
                    &area_handle,
                    &intent.quarantine_name,
                    0,
                    Path::new(""),
                    &mut counts,
                )? {
                    Removal::Gone => {
                        state::advance_intent(&write, state_paths, intent, IntentPhase::Deleted)?;
                        return Ok(ReplayOutcome::Settled);
                    }
                    Removal::Stopped(reason) => return Ok(ReplayOutcome::Pending(reason)),
                }
            }
            Ok(ReplayOutcome::Pending(
                "the leaf and marker do not prove completion".to_owned(),
            ))
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

    use super::{delete_root_on_volume, replay_pending, DeleteBounds, DeleteOutcome, ReplayReport};
    use crate::area::{AreaDeclaration, RootKind, ROOT_MARKER_FILE};
    use crate::custody::ObservedRoot;
    use crate::error::ErrorKind;
    use crate::os::DeviceId;
    use crate::state::{self, IntentPhase, IntentRecord, LockSet, QUARANTINE_PREFIX};
    use crate::testkit::{Faults, VolumeFixture};

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
            let mounts = self.fixture.mounts();
            let observer = self.fixture.observer();
            delete_root_on_volume(
                &self.fixture.state,
                dirs,
                facts,
                &mounts,
                &observer,
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
}
