//! The single-authority operation journal: one numbered record per delegated
//! or Barge-owned deletion operation, with an epoch watermark, an external
//! identity binding and fail-closed gapless reads.
//!
//! This is the persistence/integrity prerequisite of the reviewed stage 4
//! design, "One operation journal for delegated recovery"
//! (`docs/knowledge/contracts/stage4-design.md`): Barge's journal is the one
//! durable authority for an operation's progress, binding and owed caller
//! finalization. The checked begin/deletion/replay/hook coordinator is a later
//! slice; nothing here connects a caller, runs an effect, or manufactures
//! successful deletion from absence. Every numbered record is retained —
//! `cancelled` and settled `complete` records included — so the sequence stays
//! gapless and a proven loss is a detectable gap, never silence.
//!
//! The pieces, in the design's words:
//!
//! - **Epoch document** `barge.journal/1` inside the journal directory: the
//!   creation id and the `sequence` watermark. Allocation is crash-consistent:
//!   a record is published numbered `w+1` before the watermark advances to
//!   `w+1`, so a stop between the two leaves at most one fully verified
//!   allocation tail, which every later writer reconciles before it allocates.
//!   Two or more unwatermarked records, or any sequence above `w+1`, is broken
//!   allocation: unknown, refused, never renumbered.
//! - **Identity binding** `barge.journal.identity/1` outside the journal
//!   directory, at the state level: the same creation id and creation time as
//!   the epoch. It is written only by explicit initialization — epoch first,
//!   binding second, each synced — and the pair is complete only when both
//!   exist with the same id. Missing, truncated or recreated bound state is
//!   unknown; nothing is ever auto-initialized, renumbered or emptied.
//! - **Gapless reads**: binding present and readable; journal directory
//!   present; epoch present with the binding's id; watermark covering every
//!   record; every sequence `1..=watermark` present exactly once; every
//!   document within its caps. A sequence gap proves that record is lost:
//!   unknown, refused, until an operator resolves it.
//!
//! The shipped `barge.intent/2` journal is untouched: the numbered records
//! live in their own `state/operations/` directory and the two journals never
//! mix. Every journal access — read or write — establishes the actual
//! parent/directory/leaf identity through `DirOps`' descriptor-relative,
//! never-following primitives: the state directory and the journal directory
//! are opened as handles, and every document is listed, read and published
//! through the handle it was verified against, so a replaced or linked
//! directory is refused and no publication lands in a substitute. Writes are
//! capability-checked or serialized under the journal's own registry-rank
//! lock; there is no public bypass around the later checked coordinator.
//!
//! Stated detection limit, carried over from the design verbatim: an arbitrary
//! valid historical restoration — a whole-state restoration, or a valid older
//! epoch and older journal restored together while the identity binding is
//! unchanged — is indistinguishable from the journal's own past. The journal's
//! guarantees bind its own writes, not the storage beneath them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::contract::{
    INTENT_CAP_BYTES, INTENT_ROWS, JOURNAL_EPOCH_SCHEMA, JOURNAL_IDENTITY_SCHEMA,
    OPERATION_PAYLOAD_CAP_BYTES, OPERATION_SCHEMA,
};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{DirHandle, DirOps, EntryKind, ProcessObserver, VolumeUuid};
use crate::state::{
    is_identifier, now_ms, validate_entry_name, CaptureRecord, Deadline, ForeignCapture,
    IntentWrite, LockSet, StatePaths, INTENT_CAPTURES, INTENT_FOREIGN_CAPTURES, QUARANTINE_PREFIX,
    READ_DEADLINE,
};
use crate::unit::{UnitGrant, UnitKind, UnitMarker};

/// The epoch document's file name inside the journal directory.
pub const EPOCH_FILE_NAME: &str = "journal.json";
/// The identity binding's file name at the state-directory level, outside the
/// journal directory.
const IDENTITY_FILE_NAME: &str = "journal-identity.json";
/// The record files' suffix; the stem is the record's decimal sequence.
const RECORD_SUFFIX: &str = ".json";

/// Whether a string is one caller namespace: lowercase alphanumeric segments
/// joined by `.`, each 1..=32 characters, at most 64 in total.
///
/// The namespace is a report and a lookup key, never a path; the shape keeps
/// one caller from impersonating another with look-alike spelling.
fn is_caller_namespace(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    value.split('.').all(|segment| {
        !segment.is_empty()
            && segment.len() <= 32
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

/// Validate one caller namespace strictly, as a record and a request both do.
/// A request refusal is [`ErrorKind::Usage`]; a record that fails the same
/// shape is [`ErrorKind::UnknownState`].
fn validate_caller_namespace(kind: ErrorKind, field: &str, value: &str) -> Result<()> {
    if is_caller_namespace(value) {
        return Ok(());
    }
    Err(Error::new(
        kind,
        format!("`{value}` is not a `{field}` namespace of lowercase segments"),
    )
    .with_detail(field, value.to_owned().into()))
}

/// Validate one bounded name: 1..=`cap` bytes, no control characters, and
/// never a lone `.` or `..`.
fn validate_bounded_name(kind: ErrorKind, field: &str, value: &str, cap: usize) -> Result<()> {
    let usable = !value.is_empty()
        && value.len() <= cap
        && value != "."
        && value != ".."
        && !value.chars().any(char::is_control);
    if usable {
        return Ok(());
    }
    Err(Error::new(
        kind,
        format!("`{field}` must be 1 to {cap} bytes with no control character"),
    )
    .with_detail(field, value.to_owned().into()))
}

/// Validate one hook type or version name: a bounded name that is never a
/// path.
fn validate_hook_name(kind: ErrorKind, field: &str, value: &str, cap: usize) -> Result<()> {
    validate_bounded_name(kind, field, value, cap)?;
    if value.contains('/') {
        return Err(
            Error::new(kind, format!("`{field}` names one hook type, not a path"))
                .with_detail(field, value.to_owned().into()),
        );
    }
    Ok(())
}

/// The operation journal's identity binding: the creation id and creation time
/// the epoch carries, published at the state level outside the journal
/// directory so a recreated journal directory is detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalIdentity {
    /// Always `barge.journal.identity/1`.
    pub schema: String,
    /// The journal's creation id, equal to the epoch's.
    pub creation_id: String,
    /// When the journal was created, equal to the epoch's.
    pub created_ms: u64,
}

impl JournalIdentity {
    /// Validate every recorded fact strictly.
    pub fn validate(&self) -> Result<()> {
        if self.schema != JOURNAL_IDENTITY_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{}` identity binding this build reads",
                    self.schema, JOURNAL_IDENTITY_SCHEMA
                ),
            ));
        }
        if !is_identifier(&self.creation_id) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the identity binding's creation id is not 32 hex digits".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The operation journal's epoch document: the creation id and the sequence
/// watermark, published inside the journal directory by exclusive rename and
/// directory sync.
///
/// The watermark is **mutable** — every allocation advances it — while the
/// identity binding is static; the two are never called the same thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEpoch {
    /// Always `barge.journal/1`.
    pub schema: String,
    /// The journal's creation id, equal to the binding's.
    pub creation_id: String,
    /// When the journal was created, equal to the binding's.
    pub created_ms: u64,
    /// The highest sequence the journal has durably accounted for. Records are
    /// published numbered `watermark + 1` before this advances, so at most one
    /// verified unwatermarked record — the allocation tail — can exist.
    pub watermark: u64,
}

impl JournalEpoch {
    /// An epoch for `creation_id` at `created_ms`, accounting for nothing.
    pub fn new(creation_id: String, created_ms: u64) -> Self {
        Self {
            schema: JOURNAL_EPOCH_SCHEMA.to_owned(),
            creation_id,
            created_ms,
            watermark: 0,
        }
    }

    /// Validate every recorded fact strictly.
    pub fn validate(&self) -> Result<()> {
        if self.schema != JOURNAL_EPOCH_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{}` epoch major this build reads",
                    self.schema, JOURNAL_EPOCH_SCHEMA
                ),
            ));
        }
        if !is_identifier(&self.creation_id) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the epoch's creation id is not 32 hex digits".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Who resumes and finalizes one operation: the delegated caller through its
/// own durable hook, or Barge's own replay for its own roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumePolicy {
    /// The caller resumes: a delegated record, refused to generic replay and
    /// to every path except a caller-authenticated resume.
    Caller,
    /// Barge's own deleter resumes, as the shipped replay does.
    Barge,
}

/// The typed durable hook one delegated record owes: a kind, a version and a
/// bounded payload.
///
/// Barge persists type, version and payload — never closures or function
/// pointers — and validates only shape and bounds, never interpreting the
/// payload. The handler is linked code registered in-process by the later
/// coordinator; the payload budget is [`OPERATION_PAYLOAD_CAP_BYTES`], inside
/// the 4 KiB record cap, so an over-large payload or group refuses at begin
/// before any durable step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizeSpec {
    /// The hook's type name, as the registered handler reports it.
    pub kind: String,
    /// The hook version the payload was written for.
    pub version: String,
    /// The caller's own bounded payload bytes. Opaque to this store.
    pub payload: String,
}

impl FinalizeSpec {
    /// Validate shape and bounds only; the payload is never interpreted.
    pub fn validate(&self) -> Result<()> {
        validate_hook_name(ErrorKind::UnknownState, "hook kind", &self.kind, 64)?;
        validate_hook_name(ErrorKind::UnknownState, "hook version", &self.version, 32)?;
        if self.payload.len() > OPERATION_PAYLOAD_CAP_BYTES {
            return Err(Error::new(
                ErrorKind::ResourceBound,
                format!(
                    "the finalize payload is {} bytes, over the {OPERATION_PAYLOAD_CAP_BYTES}-byte payload cap",
                    self.payload.len()
                ),
            )
            .with_detail("bytes", self.payload.len().into())
            .with_detail("cap", OPERATION_PAYLOAD_CAP_BYTES.into()));
        }
        Ok(())
    }
}

/// Whether one record's caller finalization is owed, acknowledged, or released
/// by a pre-effect cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizeStatus {
    /// The durable hook is owed: the record is delegated and not terminal.
    Owed,
    /// The hook acknowledged: the record is settled at `complete`.
    Acknowledged,
    /// A pre-effect cancellation released the obligation: nothing ever ran.
    Released,
}

impl FinalizeStatus {
    /// The status's name, as a record and a report spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owed => "owed",
            Self::Acknowledged => "acknowledged",
            Self::Released => "released",
        }
    }
}

/// One operation's phase, carrying the shipped safe-deleter evidence semantics
/// into the successor major, plus the retained terminal transitions.
///
/// `intent` → `renamed` → `marker_last` → `removed` — Barge's own final unlink
/// of a verified captured leaf succeeded; recorded after that unlink, exactly
/// as shipped — → `deleted` (the durable completion fact; finalize still owed)
/// → `complete` (hook acknowledged; settled). `cancelled` is a pre-effect
/// cancellation — decided before the capture, when verification failed or the
/// re-affirmation saw a reference to the root — and is a retained terminal
/// transition, never a removal. Only `deleted` authorizes the ack; only
/// `intent`, the pre-effect world, authorizes cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    /// The record is published; no effect has happened.
    Intent,
    /// The unit was renamed to its quarantine name.
    Renamed,
    /// The marker was unlinked last and the leaf may be removed.
    MarkerLast,
    /// Barge's own final unlink of a verified captured name succeeded.
    Removed,
    /// The leaf is gone; the delegated finalize is still owed.
    Deleted,
    /// The hook acknowledged; the operation is settled.
    Complete,
    /// A pre-effect cancellation; retained, and never a removal.
    Cancelled,
}

impl OperationPhase {
    /// The phase's name, as a record and a report spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Renamed => "renamed",
            Self::MarkerLast => "marker_last",
            Self::Removed => "removed",
            Self::Deleted => "deleted",
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
        }
    }

    /// The position on the effect ladder, or `None` for a terminal phase.
    ///
    /// The ladder is the shipped order `intent` < `renamed` < `marker_last` <
    /// `removed` < `deleted`; `complete` and `cancelled` sit outside it as
    /// retained terminals, so no comparison between a terminal and a ladder
    /// phase ever orders them.
    pub const fn ladder_position(self) -> Option<u8> {
        match self {
            Self::Intent => Some(0),
            Self::Renamed => Some(1),
            Self::MarkerLast => Some(2),
            Self::Removed => Some(3),
            Self::Deleted => Some(4),
            Self::Complete | Self::Cancelled => None,
        }
    }

    /// Whether the record still needs work: a replay decision, a resume or an
    /// owed finalize.
    pub const fn is_pending(self) -> bool {
        self.ladder_position().is_some()
    }
}

/// The exact binding one operation record carries: the unit a deletion acts
/// on, resolved and verified as the plan recorded it.
///
/// Everything here is recorded before the first effect, so a replay resolves
/// the operation only from the record — the unit's kind, name, parent, device,
/// inode, marker bytes and grant, plus the volume, area folder and mount hint
/// that place it. The recorded device is the plan's within-run observation,
/// never durable identity: a replay carries the device the live volume reports
/// now, exactly as the shipped rule does, so a remount does not refuse an owed
/// operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationUnit {
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The mount point observed when the record was written; a stale hint for
    /// reports, never identity.
    pub mount_hint: PathBuf,
    /// The volume's `root_dir` the unit was resolved under.
    pub root_dir: String,
    /// The area folder the unit was resolved in.
    pub folder: String,
    /// The parent directory relative to the area folder; `""` is the folder
    /// itself.
    pub parent: String,
    /// The unit's name inside that parent.
    pub name: String,
    /// Whether the unit is a directory or one file.
    pub kind: UnitKind,
    /// The device the plan recorded; the one remount observation never bound.
    pub device: u64,
    /// The inode the plan recorded.
    pub inode: u64,
    /// The marker the plan read, when the unit carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<UnitMarker>,
    /// The marker's root id, when the unit's directory name is not it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// The private quarantine name the unit is captured under, derived from
    /// this record's own sequence and recorded before the capture. A request
    /// carries none; `begin` fills it from the allocated sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_name: Option<String>,
    /// The recorded grant the deletion rests on.
    pub grant: UnitGrant,
}

impl OperationUnit {
    /// Validate every sequence-independent recorded fact strictly. The
    /// record-level rule that binds the capture name to this record's own
    /// sequence is [`Self::validate`].
    pub fn validate_binding(&self) -> Result<()> {
        crate::unit::validate_parent(&self.parent)?;
        validate_entry_name(&self.name)?;
        if self.name == "." || self.name == ".." {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{}` is not a unit name", self.name),
            ));
        }
        self.grant.validate()?;
        if let Some(root_id) = &self.root_id {
            if !is_identifier(root_id) {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation's marker root id is not 32 lowercase hex digits".to_owned(),
                ));
            }
            if *root_id == self.name {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation records a marker root id equal to its unit name, which is one lock, not two"
                        .to_owned(),
                ));
            }
            if self.kind != UnitKind::Directory
                || !matches!(
                    self.marker.as_ref().map(|marker| marker.family),
                    Some(crate::unit::MarkerFamily::Barge)
                )
            {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "only a Barge-marked directory root can name a marker root id".to_owned(),
                ));
            }
        }
        match (&self.marker, self.kind) {
            (Some(marker), UnitKind::Directory) => marker.validate()?,
            (Some(_), UnitKind::File) => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "a file unit never carries a marker, which is a file inside a directory"
                        .to_owned(),
                ));
            }
            (None, _) => {}
        }
        crate::unit::validate_grant_shape(self.marker.as_ref(), self.kind, &self.grant, &self.name)
    }

    /// Validate every recorded fact strictly against this record's sequence.
    ///
    /// The capture name must be exactly this record's quarantine name
    /// (`.barge-prune-<sequence>`), so a record can never name another
    /// operation's private name.
    pub fn validate(&self, sequence: u64) -> Result<()> {
        let expected = format!("{QUARANTINE_PREFIX}{sequence}");
        match &self.capture_name {
            Some(capture_name) if capture_name == &expected => {}
            Some(capture_name) => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the operation's private capture name is `{capture_name}`, not `{expected}`"
                    ),
                )
                .with_detail("sequence", sequence.into()));
            }
            None => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "an operation record names no private capture name".to_owned(),
                )
                .with_detail("sequence", sequence.into()));
            }
        }
        self.validate_binding()
    }

    /// Whether two units bind the same operation: every original identity
    /// fact equal, ignoring the sequence-derived capture name.
    ///
    /// A same-key retry or a resume that presents a different inode, marker,
    /// grant, volume or request unit never continues this record; the recorded
    /// device compares too, because it is part of the request's original
    /// observation, while a *replay* is the path that tolerates a remount.
    pub fn same_binding(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.capture_name = None;
        right.capture_name = None;
        left == right
    }
}

/// One numbered `barge.operation/1` record: the single durable authority for
/// one operation's sequence, binding, caller, resume policy, typed hook, phase
/// evidence and owed or settled finalization.
///
/// The record never states a conclusion about data it has not observed, and no
/// numbered record is ever removed: `cancelled` and settled `complete`
/// records are the counted retention cost of the gapless inventory, so an
/// authorized cancellation can never masquerade as a lost record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    /// Always `barge.operation/1`.
    pub schema: String,
    /// The record's journal sequence, from `1` up to the watermark once the
    /// allocation tail is reconciled.
    pub sequence: u64,
    /// When the record was first published.
    pub recorded_ms: u64,
    /// When this phase was written.
    pub phase_ms: u64,
    /// Increases with every publication; the record starts at 1.
    pub generation: u64,
    /// Who resumes this operation.
    pub resume: ResumePolicy,
    /// The caller's namespace; present exactly when `resume` is
    /// [`ResumePolicy::Caller`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// The caller's own operation key; idempotency is per `(caller, key)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The original delegated request's digest; a resume never continues a
    /// record whose request digest differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    /// The typed durable hook the caller owes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalize: Option<FinalizeSpec>,
    /// Whether the hook is owed, acknowledged, or released by cancellation.
    pub finalize_status: FinalizeStatus,
    /// The exact unit the operation binds.
    pub unit: OperationUnit,
    /// The phase the operation reached.
    pub phase: OperationPhase,
    /// The captures this operation recorded and verified, which are the only
    /// private names it may unlink. Published before each unlink, bounded by
    /// the deletion's depth bound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<CaptureRecord>,
    /// The captures whose verification failed, which no run and no replay
    /// ever unlinks. Written before their put-back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_captures: Vec<ForeignCapture>,
}

impl OperationRecord {
    /// The delegated identity, when this record is a complete delegation.
    ///
    /// The four fields stand or fall together; a record that names a caller
    /// without its key, digest or hook fails validation before this is asked.
    fn delegation(&self) -> Option<(&str, &str, &str, &FinalizeSpec)> {
        match (
            &self.caller,
            &self.key,
            &self.request_digest,
            &self.finalize,
        ) {
            (Some(caller), Some(key), Some(digest), Some(finalize)) => {
                Some((caller.as_str(), key.as_str(), digest.as_str(), finalize))
            }
            _ => None,
        }
    }

    /// Whether the record still needs work.
    pub fn is_pending(&self) -> bool {
        self.phase.is_pending()
    }

    /// Whether the record is at a retained terminal phase.
    pub fn is_terminal(&self) -> bool {
        !self.is_pending()
    }

    /// Whether the record still excludes conflicting work: a consult, an
    /// admission or a conflicting begin refuses while a non-terminal record
    /// binds what they are about to write. Terminal records bind nothing.
    pub fn binds_conflict(&self) -> bool {
        self.is_pending()
    }

    /// Whether the record's durable hook is still owed.
    pub fn finalization_owed(&self) -> bool {
        self.finalize_status == FinalizeStatus::Owed
    }

    /// Validate every recorded fact strictly. A record that does not validate
    /// is unknown, never an operation to act on.
    pub fn validate(&self) -> Result<()> {
        if self.schema != OPERATION_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{}` major this build reads",
                    self.schema, OPERATION_SCHEMA
                ),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        if self.sequence == 0 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "an operation's sequence starts at 1".to_owned(),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        if self.generation == 0 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "an operation's generation starts at 1".to_owned(),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        if self.phase_ms < self.recorded_ms {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the operation's phase is older than the operation".to_owned(),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        self.unit.validate(self.sequence)?;
        // The delegation fields stand or fall together, and their shapes are
        // checked even on the `resume: barge` form, which must carry none.
        match (
            &self.caller,
            &self.key,
            &self.request_digest,
            &self.finalize,
        ) {
            (None, None, None, None) => {
                if self.resume != ResumePolicy::Barge {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        "a record with no delegation must resume as Barge's own".to_owned(),
                    )
                    .with_detail("sequence", self.sequence.into()));
                }
            }
            (Some(caller), Some(key), Some(digest), Some(finalize)) => {
                if self.resume != ResumePolicy::Caller {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        "a delegated record must resume as the caller's".to_owned(),
                    )
                    .with_detail("sequence", self.sequence.into()));
                }
                validate_caller_namespace(ErrorKind::UnknownState, "caller", caller)?;
                validate_bounded_name(ErrorKind::UnknownState, "key", key, 256)?;
                if !crate::unit::is_sha256(digest) {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        "the operation's request digest is not a SHA-256 hex string".to_owned(),
                    )
                    .with_detail("sequence", self.sequence.into()));
                }
                finalize.validate()?;
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "an operation's caller, key, request digest and hook are written together"
                        .to_owned(),
                )
                .with_detail("sequence", self.sequence.into()));
            }
        }
        // The finalize status and the phase agree, always.
        let expected_status = match self.phase {
            OperationPhase::Complete => FinalizeStatus::Acknowledged,
            OperationPhase::Cancelled => FinalizeStatus::Released,
            _ => FinalizeStatus::Owed,
        };
        if self.finalize_status != expected_status {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "a record at `{}` carries the finalize status `{}`, not `{}`",
                    self.phase.as_str(),
                    self.finalize_status.as_str(),
                    expected_status.as_str(),
                ),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        if self.captures.len() > INTENT_CAPTURES {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("the operation records more than {INTENT_CAPTURES} captures"),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        let quarantine = self.unit.capture_name.as_deref().unwrap_or_default();
        for capture in &self.captures {
            capture.validate(quarantine)?;
        }
        if self.foreign_captures.len() > INTENT_FOREIGN_CAPTURES {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the operation records more than {INTENT_FOREIGN_CAPTURES} foreign captures"
                ),
            )
            .with_detail("sequence", self.sequence.into()));
        }
        for foreign in &self.foreign_captures {
            foreign.validate()?;
        }
        Ok(())
    }
}

/// One fail-closed read of the whole operation journal.
///
/// `records` is sorted by sequence, is gapless from `1` through the watermark,
/// and carries the verified allocation tail when one exists. `tail_sequence`
/// names it, so a writer can reconcile the watermark before it allocates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalView {
    /// The journal's identity binding, as read.
    pub identity: JournalIdentity,
    /// The journal's epoch, as read.
    pub epoch: JournalEpoch,
    /// Every record, sorted by sequence, including a verified allocation tail.
    pub records: Vec<OperationRecord>,
    /// The sequence of the one verified unwatermarked allocation tail, when
    /// the epoch's watermark has not caught up with it yet.
    pub tail_sequence: Option<u64>,
}

impl JournalView {
    /// The record for one `(caller, key)`, when the journal holds one.
    pub fn find_operation(&self, caller: &str, key: &str) -> Option<&OperationRecord> {
        self.records.iter().find(|record| {
            record.caller.as_deref() == Some(caller) && record.key.as_deref() == Some(key)
        })
    }

    /// The non-terminal record whose owed finalize binds one original request
    /// digest, when one exists.
    pub fn find_owed_request(&self, request_digest: &str) -> Option<&OperationRecord> {
        self.records.iter().find(|record| {
            record.request_digest.as_deref() == Some(request_digest)
                && record.binds_conflict()
                && record.finalization_owed()
        })
    }

    /// The records that still exclude conflicting work.
    pub fn nonterminal(&self) -> impl Iterator<Item = &OperationRecord> {
        self.records.iter().filter(|record| record.binds_conflict())
    }
}

/// The answer to a begin: a freshly allocated operation, or the existing one a
/// same-key request binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// A new record was published durably at phase `intent`.
    Began(OperationRecord),
    /// An existing record with the same caller, key, request and hook is
    /// returned unchanged; idempotency binds one operation per `(caller, key)`.
    Already(OperationRecord),
}

impl Begun {
    /// The record this begin answers with, either way.
    pub fn record(&self) -> &OperationRecord {
        match self {
            Self::Began(record) | Self::Already(record) => record,
        }
    }
}

/// A begin request: who resumes, the delegated identity when the caller does,
/// and the exact unit binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeginRequest {
    /// Who resumes the operation.
    pub resume: ResumePolicy,
    /// The caller's namespace, when the caller resumes.
    pub caller: Option<String>,
    /// The caller's own operation key, when the caller resumes.
    pub key: Option<String>,
    /// The original delegated request's digest, when the caller resumes.
    pub request_digest: Option<String>,
    /// The typed durable hook, when the caller resumes.
    pub finalize: Option<FinalizeSpec>,
    /// The exact unit binding, without its sequence-derived capture name yet.
    pub unit: OperationUnit,
}

impl BeginRequest {
    /// A delegated begin: the caller resumes through its own hook.
    pub fn delegated(
        caller: impl Into<String>,
        key: impl Into<String>,
        request_digest: impl Into<String>,
        finalize: FinalizeSpec,
        unit: OperationUnit,
    ) -> Self {
        Self {
            resume: ResumePolicy::Caller,
            caller: Some(caller.into()),
            key: Some(key.into()),
            request_digest: Some(request_digest.into()),
            finalize: Some(finalize),
            unit,
        }
    }

    /// A Barge-owned begin: Barge's own replay resumes its own root.
    pub fn barge_owned(unit: OperationUnit) -> Self {
        Self {
            resume: ResumePolicy::Barge,
            caller: None,
            key: None,
            request_digest: None,
            finalize: None,
            unit,
        }
    }

    /// Validate every presented fact strictly, before any durable step.
    ///
    /// The capture name must not be pre-set: it is derived from the allocated
    /// sequence, so a request can never name another operation's private name.
    pub fn validate(&self) -> Result<()> {
        match (
            &self.caller,
            &self.key,
            &self.request_digest,
            &self.finalize,
        ) {
            (Some(caller), Some(key), Some(digest), Some(finalize)) => {
                if self.resume != ResumePolicy::Caller {
                    return Err(Error::new(
                        ErrorKind::Usage,
                        "a delegated begin must ask for the caller to resume".to_owned(),
                    ));
                }
                validate_caller_namespace(ErrorKind::UnknownState, "caller", caller)?;
                validate_bounded_name(ErrorKind::UnknownState, "key", key, 256)?;
                if !crate::unit::is_sha256(digest) {
                    return Err(Error::new(
                        ErrorKind::Usage,
                        "the request digest is not a SHA-256 hex string".to_owned(),
                    ));
                }
                finalize.validate()?;
            }
            (None, None, None, None) => {
                if self.resume != ResumePolicy::Barge {
                    return Err(Error::new(
                        ErrorKind::Usage,
                        "a begin with no delegation must ask for Barge's own replay".to_owned(),
                    ));
                }
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Usage,
                    "a begin's caller, key, request digest and hook are presented together"
                        .to_owned(),
                ));
            }
        }
        if self.unit.capture_name.is_some() {
            return Err(Error::new(
                ErrorKind::Usage,
                "a begin request derives the private capture name from the allocated sequence; it never accepts one"
                    .to_owned(),
            ));
        }
        self.unit.validate_binding()
    }
}

/// What one entry name inside the journal directory can be.
enum NameClass {
    /// The epoch document.
    Epoch,
    /// One canonical numbered record: decimal, no leading zeros.
    Record(u64),
    /// Anything else, which is unknown state.
    Invalid,
}

/// Classify one journal directory entry name.
fn classify_name(name: &str) -> NameClass {
    if name == EPOCH_FILE_NAME {
        return NameClass::Epoch;
    }
    let Some(sequence_text) = name.strip_suffix(RECORD_SUFFIX) else {
        return NameClass::Invalid;
    };
    if sequence_text.is_empty()
        || !sequence_text.bytes().all(|byte| byte.is_ascii_digit())
        || (sequence_text.len() > 1 && sequence_text.starts_with('0'))
    {
        return NameClass::Invalid;
    }
    sequence_text
        .parse()
        .map(NameClass::Record)
        .unwrap_or(NameClass::Invalid)
}

/// Where one numbered record lives: the canonical decimal sequence, inside the
/// journal directory. Fixture crafting uses it; every production access goes
/// through an opened [`JournalRoot`] instead of a path.
#[cfg(test)]
fn record_path(state: &StatePaths, sequence: u64) -> PathBuf {
    state
        .operations_dir()
        .join(format!("{sequence}{RECORD_SUFFIX}"))
}

/// Where the epoch document lives inside the journal directory.
#[cfg(test)]
fn epoch_path(state: &StatePaths) -> PathBuf {
    state.operations_dir().join(EPOCH_FILE_NAME)
}

/// One journal document's published file mode, as the state layer's own
/// documents are published: private to the owner.
const DOCUMENT_MODE: u32 = 0o600;

/// How many entry names the empty-journal probe may list before it stops:
/// the probe answers "is there anything to account for", so a journal holding
/// more than this at initialization time is exactly the refuse case, and the
/// probe reports it as such without reading a byte.
const PROBE_LIST_CAP: usize = 64;

/// The opened custody of one journal access.
///
/// The state directory and the journal directory are opened through
/// [`DirOps`]' descriptor-relative primitives — `openat` with `O_NOFOLLOW`
/// (and `O_DIRECTORY` for directories) — and every later list, read or
/// publish goes through the handle this struct holds, never through a path
/// again. A symbolic link or a non-directory at either name is refused
/// before it is ever followed, and a directory swapped in after the open
/// leaves this struct acting on the object it verified, never on the
/// substitute. This is the parent/directory/leaf identity boundary: a leaf
/// check alone cannot establish it, and no metadata precheck claims to close
/// a race — the handle is the custody.
///
/// The owner boundary is the private permission set: a journal directory or
/// document openable by group or other is not the state Barge published. The
/// state directory is created `0700` and its documents `0600`; anything else
/// is refused, never repaired.
struct JournalRoot<'a> {
    dirs: &'a dyn DirOps,
    /// The trusted current account: the platform's own effective-uid fact,
    /// read once at open and compared against every owner fact below. Never a
    /// uid from a request, a record or a path.
    current_uid: u32,
    /// The state directory's handle; `None` when the state directory itself
    /// is absent, which reads as an unbound journal and refuses every write.
    state_root: Option<DirHandle>,
    /// The journal directory's handle; `None` when nothing is at its name.
    operations: Option<DirHandle>,
}

/// Verify one opened directory handle is a real, private directory owned by
/// the trusted current account.
fn verify_private_directory(
    dirs: &dyn DirOps,
    handle: &DirHandle,
    name: &'static str,
    current_uid: u32,
) -> Result<()> {
    let facts = dirs.dir_facts(handle)?;
    if facts.kind != EntryKind::Directory {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("{name} is not a directory; it is never followed"),
        ));
    }
    if facts.mode & 0o077 != 0 {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{name} is not private: its mode {:o} opens it to group or other",
                facts.mode & 0o777
            ),
        ));
    }
    verify_owner(facts.uid, current_uid, name)
}

/// Compare one owner fact with the trusted current account. Private modes are
/// never ownership evidence: a `0700` directory owned by another account is
/// someone else's data, and the journal refuses it rather than reading it.
fn verify_owner(facts_uid: u32, current_uid: u32, name: &str) -> Result<()> {
    if facts_uid == current_uid {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::ForeignOwner,
        format!(
            "{name} is owned by uid {facts_uid}, not by this account (uid {current_uid}); another account's journal state is refused, never read or written"
        ),
    )
    .with_detail("owner_uid", facts_uid.into())
    .with_detail("current_uid", current_uid.into()))
}

/// Open the journal's custody: the state directory, then the journal
/// directory under it, each verified as a real private directory through its
/// own handle.
///
/// Absence is distinguishable from refusal: a missing state directory or a
/// missing journal directory opens as `None` at that level (an unbound
/// journal reads as such), while a symbolic link, another kind, or an
/// unsafe mode at either name is an [`ErrorKind::UnknownState`] refusal.
fn open_journal_root<'a>(
    dirs: &'a dyn DirOps,
    observer: &'a dyn ProcessObserver,
    state: &StatePaths,
) -> Result<JournalRoot<'a>> {
    // The trusted current account, read once from the platform's own fact.
    // An undecided account refuses every access: unknown is never a match.
    let current_uid = observer.effective_uid().map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "the executing account cannot be decided, so journal ownership cannot be compared: {error}"
            ),
        )
    })?;
    let state_root = match dirs.path_facts(state.root())? {
        None => None,
        Some(facts) if facts.kind == EntryKind::Directory => {
            let handle = dirs.open_path(state.root())?;
            verify_private_directory(dirs, &handle, "the state directory", current_uid)?;
            Some(handle)
        }
        Some(_) => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "{} is not a directory; a state directory is never followed through anything else",
                    state.root().display()
                ),
            )
            .with_detail("path", state.root().display().to_string().into()));
        }
    };
    let operations = match (&state_root, dirs.path_facts(&state.operations_dir())?) {
        (None, _) | (_, None) => None,
        (Some(root), Some(facts)) if facts.kind == EntryKind::Directory => {
            let handle = dirs.open_child_dir(root, "operations")?;
            verify_private_directory(
                dirs,
                &handle,
                "the operation journal directory",
                current_uid,
            )?;
            Some(handle)
        }
        (Some(_), Some(_)) => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "{} is not a directory; a journal directory is never followed through anything else",
                    state.operations_dir().display()
                ),
            )
            .with_detail("path", state.operations_dir().display().to_string().into()));
        }
    };
    Ok(JournalRoot {
        dirs,
        current_uid,
        state_root,
        operations,
    })
}

impl JournalRoot<'_> {
    /// The journal directory's handle, or the refusal every writer owes an
    /// uncreated journal: the state layout must exist first, and this store
    /// creates no directory.
    fn operations_handle(&self) -> Result<&DirHandle> {
        self.operations.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                "the operation journal directory does not exist; the state layout must be initialized first, and this store never creates it"
                    .to_owned(),
            )
        })
    }

    /// Read one child document through its parent handle, private and
    /// never-following: the leaf's own facts are taken first (no-follow), a
    /// non-regular file or an exposed mode refuses, and only then is the
    /// body read through the same handle, where the platform re-verifies
    /// identity and refuses a link again. `Ok(None)` is absent, at the leaf
    /// or vanished between the facts and the read.
    fn read_child_document(&self, parent: &DirHandle, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(facts) = self.dirs.child_facts(parent, name)? else {
            return Ok(None);
        };
        if facts.kind != EntryKind::File {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{name}` in the journal is not a regular file; a journal document is never followed"
                ),
            ));
        }
        if facts.mode & 0o077 != 0 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{name}` in the journal is not private: its mode {:o} opens it to group or other",
                    facts.mode & 0o777
                ),
            ));
        }
        // A verified parent never proves a child's owner: every leaf is
        // compared with the trusted account on its own facts.
        verify_owner(
            facts.uid,
            self.current_uid,
            &format!("the journal document `{name}`"),
        )?;
        self.dirs.read_child_file(parent, name, INTENT_CAP_BYTES)
    }

    /// Read the identity binding through the state directory's handle.
    fn read_identity(&self) -> Result<Option<JournalIdentity>> {
        let Some(root) = self.state_root.as_ref() else {
            return Ok(None);
        };
        match self.read_child_document(root, IDENTITY_FILE_NAME)? {
            None => Ok(None),
            Some(bytes) => {
                let identity: JournalIdentity = parse_schema(
                    &bytes,
                    Path::new(IDENTITY_FILE_NAME),
                    JOURNAL_IDENTITY_SCHEMA,
                )?;
                identity.validate()?;
                Ok(Some(identity))
            }
        }
    }

    /// Read the epoch document through the journal directory's handle.
    fn read_epoch(&self) -> Result<Option<JournalEpoch>> {
        let Some(operations) = self.operations.as_ref() else {
            return Ok(None);
        };
        match self.read_child_document(operations, EPOCH_FILE_NAME)? {
            None => Ok(None),
            Some(bytes) => {
                let epoch: JournalEpoch =
                    parse_schema(&bytes, Path::new(EPOCH_FILE_NAME), JOURNAL_EPOCH_SCHEMA)?;
                epoch.validate()?;
                Ok(Some(epoch))
            }
        }
    }

    /// Whether the journal directory holds any record-named entry, for the
    /// empty-journal probes.
    ///
    /// The question these probes answer is "is there anything to account
    /// for", never "how much", so the listing stops once the answer is known
    /// and never reports a row bound: a journal that is supposed to be
    /// genuinely empty but holds anything else is unknown state, and the
    /// caller refuses it as one.
    fn has_record_entry(&self) -> Result<bool> {
        let Some(operations) = self.operations.as_ref() else {
            return Ok(false);
        };
        let names = self.dirs.list_names(operations, PROBE_LIST_CAP)?;
        if names.truncated {
            return Ok(true);
        }
        for name in &names.names {
            if matches!(
                classify_name(name),
                NameClass::Record(_) | NameClass::Invalid
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// List the journal directory's record entries through its handle,
    /// bounded, in sequence order.
    ///
    /// Every entry name must be the epoch document or one canonical numbered
    /// record name; anything else — a stray temporary, a foreign document, a
    /// name with leading zeros — is unknown state, never skipped. The listing
    /// itself is descriptor-relative: it lists the directory this struct
    /// opened and verified, whatever the name holds now.
    fn list_record_entries(&self, max: usize, deadline: Deadline) -> Result<Vec<(String, u64)>> {
        let Some(operations) = self.operations.as_ref() else {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the operation journal directory is gone; a bound journal is never recreated"
                    .to_owned(),
            ));
        };
        // The listing holds the epoch document too, so the name cap carries
        // one extra slot for it; the record bound itself is checked after the
        // epoch is filtered out.
        let names = self.dirs.list_names(operations, max.saturating_add(1))?;
        if names.truncated {
            return Err(Error::new(
                ErrorKind::ResourceBound,
                format!(
                    "the operation journal holds more than {max} record files; the bound is refused, never truncated"
                ),
            )
            .with_detail("cap", max.into()));
        }
        let mut entries = Vec::new();
        for name in names.names {
            let sequence = match classify_name(&name) {
                NameClass::Epoch => continue,
                NameClass::Record(sequence) => sequence,
                NameClass::Invalid => {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        format!(
                            "the operation journal holds `{name}`, which is not the epoch document or one canonical numbered record"
                        ),
                    ));
                }
            };
            entries.push((name, sequence));
            if entries.len() > max {
                return Err(Error::new(
                    ErrorKind::ResourceBound,
                    format!(
                        "the operation journal holds more than {max} record files; the bound is refused, never truncated"
                    ),
                )
                .with_detail("cap", max.into()));
            }
            if deadline.expired() {
                return Err(Error::new(
                    ErrorKind::ResourceBound,
                    "the operation journal read deadline expired",
                ));
            }
        }
        entries.sort_by_key(|(_, sequence)| *sequence);
        Ok(entries)
    }

    /// Read every numbered record strictly, bounded, in sequence order.
    ///
    /// A record that vanished between the listing and the read is a breakage,
    /// never skipped: no numbered record is ever removed by design, so a
    /// disappearance is exactly the loss the gapless rule must surface.
    fn read_records(
        &self,
        entries: &[(String, u64)],
        deadline: Deadline,
    ) -> Result<Vec<OperationRecord>> {
        let operations = self.operations_handle()?;
        let mut records = Vec::with_capacity(entries.len());
        for (name, sequence) in entries {
            if deadline.expired() {
                return Err(Error::new(
                    ErrorKind::ResourceBound,
                    "the operation journal read deadline expired",
                ));
            }
            match self.read_child_document(operations, name)? {
                Some(bytes) => {
                    let path = Path::new(name);
                    let record: OperationRecord = parse_schema(&bytes, path, OPERATION_SCHEMA)?;
                    if record.sequence != *sequence {
                        return Err(Error::new(
                            ErrorKind::UnknownState,
                            format!(
                                "the operation journal record `{name}` carries sequence {}, which is not its file name",
                                record.sequence
                            ),
                        )
                        .with_detail("sequence", record.sequence.into()));
                    }
                    record.validate()?;
                    records.push(record);
                }
                None => {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        format!(
                            "the operation journal record `{name}` vanished between its listing and its read"
                        ),
                    ));
                }
            }
        }
        Ok(records)
    }

    /// Publish one journal document durably through its parent handle: a
    /// private temporary file in the verified directory, synced, renamed into
    /// place, and the directory synced — the platform's own atomic
    /// publication, descriptor-relative, so the bytes land in the directory
    /// this struct verified and never in a substitute.
    fn publish_document(&self, parent: &DirHandle, name: &str, bytes: &[u8]) -> Result<()> {
        #[cfg(any(test, feature = "proof-journal"))]
        {
            if let Some(error) = FAIL_DOCUMENT_PUBLISH.with(|cell| cell.borrow_mut().take()) {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("cannot publish `{name}`: {error}"),
                ));
            }
        }
        self.dirs
            .publish_child_file(parent, name, bytes, DOCUMENT_MODE)
    }
}

/// Parse one JSON document that must be exactly the expected schema's shape.
fn parse_schema<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    path: &Path,
    expected: &str,
) -> Result<T> {
    let document: T = serde_json::from_slice(bytes).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!("{} is not a `{expected}` document: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    Ok(document)
}

/// Serialize one journal document to its canonical bytes, inside the record
/// cap, ending with one newline.
fn document_bytes<T: Serialize>(document: &T, name: &str) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the {name}: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > INTENT_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the {name} would be {} bytes, over the {INTENT_CAP_BYTES}-byte cap",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

#[cfg(any(test, feature = "proof-journal"))]
thread_local! {
    static FAIL_DOCUMENT_PUBLISH: std::cell::RefCell<Option<std::io::Error>> =
        const { std::cell::RefCell::new(None) };
    static FAIL_WATERMARK_PUBLISH: std::cell::RefCell<Option<std::io::Error>> =
        const { std::cell::RefCell::new(None) };
}

/// Arm one injected publication failure at the next journal document
/// publication, before any effect, on this thread.
///
/// Proof and test only: armed by this crate's own regressions and, under the
/// proof-only `proof-journal` feature, by the staged proof driver
/// (`scripts/native/journal-proof.py`), which labels every injected failure
/// in its case evidence. No ordinary build enables the feature, so an
/// ordinary release carries neither the endpoint nor the seam it arms, and
/// this function arms a refusal - it never writes anything itself.
#[cfg(any(test, feature = "proof-journal"))]
pub fn fail_next_document_publish(error: std::io::Error) {
    FAIL_DOCUMENT_PUBLISH.with(|cell| *cell.borrow_mut() = Some(error));
}

/// Arm one injected publication failure at the next watermark advance, so a
/// proof or test can stop the allocation exactly between the record
/// publication and the watermark that accounts for it. Proof and test only,
/// under the same feature gate as [`fail_next_document_publish`].
#[cfg(any(test, feature = "proof-journal"))]
pub fn fail_next_watermark_publish(error: std::io::Error) {
    FAIL_WATERMARK_PUBLISH.with(|cell| *cell.borrow_mut() = Some(error));
}

/// The gapless accounting: every sequence `1..=watermark` exactly once, plus
/// at most one verified allocation tail at `watermark + 1`. Any gap, duplicate
/// identity, unexpected sequence or multi-tail is named, and refused.
fn account_sequences(records: &[OperationRecord], watermark: u64) -> Result<Option<u64>> {
    let mut expected: u64 = 1;
    let mut tail: Option<u64> = None;
    for record in records {
        let sequence = record.sequence;
        if sequence <= watermark {
            if sequence != expected {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the operation journal's record {expected} is missing: a sequence gap is proven loss, never silence"
                    ),
                )
                .with_detail("sequence", expected.into())
                .with_detail("watermark", watermark.into()));
            }
            expected += 1;
        } else if sequence == watermark.saturating_add(1) {
            if tail.is_some() {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal holds more than one unwatermarked record; broken allocation is never renumbered",
                )
                .with_detail("watermark", watermark.into())
                .with_detail("sequence", sequence.into()));
            }
            tail = Some(sequence);
        } else {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the operation journal holds record {sequence}, which is beyond watermark {watermark} plus one allocation tail"
                ),
            )
            .with_detail("sequence", sequence.into())
            .with_detail("watermark", watermark.into()));
        }
    }
    if expected <= watermark {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the operation journal's record {expected} is missing: a sequence gap is proven loss, never silence"
            ),
        )
        .with_detail("sequence", expected.into())
        .with_detail("watermark", watermark.into()));
    }
    Ok(tail)
}

/// Read the whole operation journal, fail-closed, bounded, without writing.
///
/// The returned view is authoritative: every sequence through the watermark is
/// present exactly once, and a single verified allocation tail at
/// `watermark + 1` is included and named. Any gap, duplicate identity,
/// malformed or over-cap document, unexpected sequence, unknown format or
/// broken pair is an [`ErrorKind::UnknownState`] or
/// [`ErrorKind::ResourceBound`] refusal — never a short answer, never a
/// renumbering, never an implicit repair. Every access goes through the
/// opened [`JournalRoot`]: the journal directory is never re-resolved by path
/// after it is verified, and a replaced or linked directory is refused.
///
/// `Ok(None)` is the one defined non-answer: the journal has never been
/// initialized (no binding and no epoch, and no records either), so there is
/// no authority to read rather than authority that failed to read.
pub fn read_operations(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    max: usize,
) -> Result<Option<JournalView>> {
    read_operations_with_deadline(dirs, observer, state, max, Deadline::after(READ_DEADLINE))
}

/// The body of [`read_operations`] with the caller's remaining budget.
pub(crate) fn read_operations_with_deadline(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    max: usize,
    deadline: Deadline,
) -> Result<Option<JournalView>> {
    if deadline.expired() {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "the operation journal read deadline expired",
        ));
    }
    let root = open_journal_root(dirs, observer, state)?;
    let identity = root.read_identity()?;
    let epoch = root.read_epoch()?;
    let (identity, epoch) = match (identity, epoch) {
        (None, None) => {
            // Never initialized. The journal directory must hold nothing: a
            // record without a complete pair is exactly the state the binding
            // exists to detect, never a clean empty journal.
            if root.has_record_entry()? {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal holds records but no identity binding or epoch; it is never bound after the fact"
                        .to_owned(),
                ));
            }
            return Ok(None);
        }
        (Some(identity), None) => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the operation journal's identity binding exists but its epoch document is missing; a destroyed or recreated journal is unknown, never re-initialized"
                    .to_owned(),
            )
            .with_detail("creation_id", identity.creation_id.into()));
        }
        (None, Some(epoch)) => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the operation journal's epoch exists but its identity binding is missing; the authority is incomplete and stays blocked"
                    .to_owned(),
            )
            .with_detail("creation_id", epoch.creation_id.into()));
        }
        (Some(identity), Some(epoch)) => {
            if identity.creation_id != epoch.creation_id || identity.created_ms != epoch.created_ms
            {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal's identity binding and epoch disagree; the pair is incomplete"
                        .to_owned(),
                )
                .with_detail("identity_creation_id", identity.creation_id.into())
                .with_detail("epoch_creation_id", epoch.creation_id.into()));
            }
            (identity, epoch)
        }
    };
    let entries = root.list_record_entries(max, deadline)?;
    let records = root.read_records(&entries, deadline)?;
    let tail_sequence = account_sequences(&records, epoch.watermark)?;
    Ok(Some(JournalView {
        identity,
        epoch,
        records,
        tail_sequence,
    }))
}

/// The record file name for one sequence, as the journal's documents are
/// named inside the verified journal directory.
fn record_file_name(sequence: u64) -> String {
    format!("{sequence}{RECORD_SUFFIX}")
}

/// Publish the identity binding durably, through the state directory's
/// verified handle.
fn publish_identity(root: &JournalRoot<'_>, binding: &JournalIdentity) -> Result<()> {
    let handle = root.state_root.as_ref().ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "the state directory does not exist; the layout must be initialized first",
        )
    })?;
    root.publish_document(
        handle,
        IDENTITY_FILE_NAME,
        &document_bytes(binding, "identity binding")?,
    )
}

/// Advance one epoch's watermark, under the journal's exclusive lock only,
/// through the verified journal directory's handle.
fn advance_watermark(root: &JournalRoot<'_>, epoch: &JournalEpoch, watermark: u64) -> Result<()> {
    #[cfg(any(test, feature = "proof-journal"))]
    {
        if let Some(error) = FAIL_WATERMARK_PUBLISH.with(|cell| cell.borrow_mut().take()) {
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot publish the epoch document: {error}"),
            ));
        }
    }
    let mut next = epoch.clone();
    next.watermark = watermark;
    let operations = root.operations_handle()?;
    root.publish_document(
        operations,
        EPOCH_FILE_NAME,
        &document_bytes(&next, "epoch document")?,
    )
}

/// Initialize the operation journal explicitly: publish the epoch document
/// first, then the identity binding, each synced, under the journal's lock.
///
/// The rules, exactly as the design states them:
///
/// - Neither document exists and the journal directory holds no records: this
///   is the creation. One fresh creation id and creation time go into both
///   documents, epoch first, binding second.
/// - The epoch exists, the binding is missing, and the journal is genuinely
///   empty (watermark 0, no records): a previous initialization stopped
///   between its two publications, and an explicit re-run may finish it, with
///   the epoch's own creation id and creation time.
/// - Both exist with the same id and creation time: initialization is already
///   complete, and this is a no-op.
/// - Anything else — a binding without an epoch, a pair that disagrees, an
///   unreadable or over-cap document, records without a complete pair — is
///   unknown or refused, and nothing is created, re-created, re-numbered or
///   repaired implicitly.
///
/// The state directory's layout must already exist ([`StatePaths::initialize`]
/// creates it); this call creates no directory and never auto-initializes.
pub fn init_operation_journal(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    locks: &mut LockSet,
) -> Result<()> {
    let _lock = locks.lock_operations()?;
    let root = open_journal_root(dirs, observer, state)?;
    let identity = root.read_identity()?;
    let epoch = root.read_epoch()?;
    match (identity, epoch) {
        (Some(identity), Some(epoch)) => {
            if identity.creation_id != epoch.creation_id || identity.created_ms != epoch.created_ms
            {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal is already initialized with a pair that disagrees; it is never repaired implicitly"
                        .to_owned(),
                )
                .with_detail("identity_creation_id", identity.creation_id.into())
                .with_detail("epoch_creation_id", epoch.creation_id.into()));
            }
            Ok(())
        }
        (Some(identity), None) => Err(Error::new(
            ErrorKind::UnknownState,
            "the operation journal's identity binding exists but its epoch document is missing; a destroyed or recreated journal is unknown, never re-initialized"
                .to_owned(),
        )
        .with_detail("creation_id", identity.creation_id.into())),
        (None, Some(epoch)) => {
            // A previous initialization stopped between its two publications.
            // It may finish only a genuinely empty journal.
            if epoch.watermark != 0 {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal's epoch accounts for records its missing identity binding never bound; initialization never finishes a journal that is not genuinely empty"
                        .to_owned(),
                )
                .with_detail("creation_id", epoch.creation_id.into())
                .with_detail("watermark", epoch.watermark.into()));
            }
            if root.has_record_entry()? {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal holds records its missing identity binding never bound; initialization never finishes a journal that is not genuinely empty"
                        .to_owned(),
                )
                .with_detail("creation_id", epoch.creation_id.into()));
            }
            publish_identity(
                &root,
                &JournalIdentity {
                    schema: JOURNAL_IDENTITY_SCHEMA.to_owned(),
                    creation_id: epoch.creation_id.clone(),
                    created_ms: epoch.created_ms,
                },
            )
        }
        (None, None) => {
            // The journal directory must exist: the state layout creates it,
            // and this store never creates a directory itself.
            let operations = root.operations_handle()?;
            if root.has_record_entry()? {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the operation journal holds records but no identity binding or epoch; it is never bound after the fact"
                        .to_owned(),
                ));
            }
            let creation_id = hex(&dirs.random_bytes(16)?);
            let created_ms = now_ms();
            // Epoch first, binding second, each synced; the pair is complete
            // only when both exist with the same id. A stop between the two is
            // the epoch-only state an explicit re-run may finish above.
            root.publish_document(
                operations,
                EPOCH_FILE_NAME,
                &document_bytes(&JournalEpoch::new(creation_id.clone(), created_ms), "epoch document")?,
            )?;
            publish_identity(
                &root,
                &JournalIdentity {
                    schema: JOURNAL_IDENTITY_SCHEMA.to_owned(),
                    creation_id,
                    created_ms,
                },
            )
        }
    }
}

/// The failure a begin reports when the journal has not been initialized.
fn uninitialized_error() -> Error {
    Error::new(
        ErrorKind::UnknownState,
        "the operation journal is not initialized; it is never initialized implicitly, and explicit initialization comes first"
            .to_owned(),
    )
}

/// Publish one operation record durably: encode, refuse the record cap, and
/// publish atomically. Capability-checked: the caller holds the intent-write
/// step, last in the fixed lock order.
fn publish_operation(
    _write: &IntentWrite,
    root: &JournalRoot<'_>,
    record: &OperationRecord,
) -> Result<()> {
    record.validate()?;
    let bytes = document_bytes(record, "operation record")?;
    let operations = root.operations_handle()?;
    root.publish_document(operations, &record_file_name(record.sequence), &bytes)
}

/// Begin one operation under the journal's exclusive lock: reconcile any
/// allocation tail, refuse when the journal is at capacity, answer an exact
/// same-key request with its existing record, and otherwise allocate the next
/// sequence, publish the record at phase `intent`, and advance the watermark.
///
/// Idempotency and refusal rules, exactly as the reviewed contract names them:
///
/// - The same `(caller, key)` with the same original request digest, unit
///   binding and hook kind, version and payload answers [`Begun::Already`]
///   with the existing record; a `cancelled` key is spent and refuses, because
///   a fresh attempt uses a fresh key.
/// - The same `(caller, key)` with any mismatched digest, unit, hook kind,
///   version or payload refuses: a matching caller id alone never accepts a
///   mismatched request.
/// - A different `(caller, key)` whose request digest matches a record with an
///   owed finalize refuses: one request binds one operation while it is owed.
/// - A unit a non-terminal record binds is spoken for: a Barge-owned begin is
///   idempotent on the exact unit binding, and any other begin for that unit
///   refuses, whatever resume policy presents it.
/// - An uninitialized journal refuses; it is never initialized implicitly.
/// - The capacity refusal is the **new-allocation** gate, answered last: at a
///   full but valid journal, an exact same-key or same-unit request still
///   returns its existing record, and a mismatched or conflicting request
///   still refuses as such. Only a genuinely new operation at the cap
///   refuses, without a write.
///
/// A record whose publication fails reports the failure: the allocation is
/// unresolved, the record may or may not exist, and a later same-key begin
/// binds the one operation either way. A record whose **watermark advance**
/// fails after its own publication succeeded still answers [`Begun::Began`] —
/// the record is confirmed durable and every writer reconciles the verified
/// tail before it allocates.
pub fn begin_operation(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    locks: &mut LockSet,
    request: BeginRequest,
) -> Result<Begun> {
    request.validate()?;
    let _lock = locks.lock_operations()?;
    let deadline = Deadline::after(READ_DEADLINE);
    let root = open_journal_root(dirs, observer, state)?;
    let Some(view) = read_operations_with_deadline(dirs, observer, state, INTENT_ROWS, deadline)?
    else {
        return Err(uninitialized_error());
    };
    // Crash-consistent allocation: the tail is reconciled before anything
    // allocates on top of it, so two unwatermarked records can never exist.
    let mut watermark = view.epoch.watermark;
    if let Some(tail) = view.tail_sequence {
        advance_watermark(&root, &view.epoch, tail)?;
        watermark = tail;
    }
    // Idempotency and conflicts are answered before the capacity refusal, so
    // a full journal never prevents the retry of an operation that already
    // exists: matching requests and Barge-owned units return their exact
    // existing records, and only a genuinely NEW allocation at the cap
    // refuses, writing nothing.
    if let (ResumePolicy::Caller, Some(caller), Some(key)) = (
        request.resume,
        request.caller.as_deref(),
        request.key.as_deref(),
    ) {
        if let Some(existing) = view.find_operation(caller, key) {
            if !same_request(existing, &request) {
                return Err(Error::new(
                    ErrorKind::RefusedBoundary,
                    format!(
                        "the operation key `{key}` for caller `{caller}` already binds a record whose request, unit or hook differs; a matching caller id alone never accepts a mismatched request"
                    ),
                )
                .with_detail("sequence", existing.sequence.into())
                .with_detail("phase", existing.phase.as_str().into()));
            }
            if existing.phase == OperationPhase::Cancelled {
                return Err(Error::new(
                    ErrorKind::RefusedBoundary,
                    format!(
                        "the operation key `{key}` for caller `{caller}` was cancelled; a fresh attempt uses a fresh key"
                    ),
                )
                .with_detail("sequence", existing.sequence.into()));
            }
            return Ok(Begun::Already(existing.clone()));
        }
    }
    if let Some(digest) = request.request_digest.as_deref() {
        if let Some(existing) = view.find_owed_request(digest) {
            let same_operation = existing.caller.as_deref() == request.caller.as_deref()
                && existing.key.as_deref() == request.key.as_deref();
            if !same_operation {
                return Err(Error::new(
                    ErrorKind::RefusedBoundary,
                    "the original request already binds an operation whose finalize is owed; a second operation never begins for it"
                        .to_owned(),
                )
                .with_detail("sequence", existing.sequence.into()));
            }
        }
    }
    // One unit carries at most one non-terminal operation. A Barge-owned
    // begin is idempotent on the exact unit binding - the unit is its own
    // operation key - and any other begin for a unit a pending record binds
    // refuses, whoever resumes it: two pending operations must never point at
    // one unit. A terminal record binds nothing, so a fresh attempt after a
    // cancellation may begin again.
    if let Some(existing) = view
        .records
        .iter()
        .find(|record| record.binds_conflict() && record.unit.same_binding(&request.unit))
    {
        if request.resume == ResumePolicy::Barge && existing.resume == ResumePolicy::Barge {
            return Ok(Begun::Already(existing.clone()));
        }
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "the unit `{}` is already bound to the pending operation at sequence {}; a second operation never begins for it",
                existing.unit.name, existing.sequence
            ),
        )
        .with_detail("sequence", existing.sequence.into())
        .with_detail("phase", existing.phase.as_str().into()));
    }
    // Capacity is the new-allocation gate, checked last: every answer above
    // either returned an existing record or refused a conflict, and none of
    // them allocates.
    if watermark >= INTENT_ROWS as u64 {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the operation journal holds {watermark} retained records, at its {INTENT_ROWS}-record cap; it refuses further work, never evicts"
            ),
        )
        .with_detail("watermark", watermark.into())
        .with_detail("cap", INTENT_ROWS.into()));
    }
    let sequence = watermark.checked_add(1).ok_or_else(|| {
        Error::new(
            ErrorKind::ResourceBound,
            "the journal's sequence space overflowed",
        )
    })?;
    let now = now_ms();
    let mut unit = request.unit.clone();
    unit.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
    let record = OperationRecord {
        schema: OPERATION_SCHEMA.to_owned(),
        sequence,
        recorded_ms: now,
        phase_ms: now,
        generation: 1,
        resume: request.resume,
        caller: request.caller.clone(),
        key: request.key.clone(),
        request_digest: request.request_digest.clone(),
        finalize: request.finalize.clone(),
        finalize_status: FinalizeStatus::Owed,
        unit,
        phase: OperationPhase::Intent,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
    };
    // A token is required even for the allocation write: the intent-write step
    // is the type-level proof that no effect may run before this publish.
    let write = locks.begin_intent_write()?;
    publish_operation(&write, &root, &record)?;
    // The record is durably published; the watermark advance is the priced
    // machinery of proven-loss detection. If it fails, the journal is in the
    // valid allocation-tail state every writer reconciles, and the begin still
    // answers with its confirmed record.
    let _ = advance_watermark(&root, &view.epoch, sequence);
    Ok(Begun::Began(record))
}

/// Whether an existing record and a begin request present the same operation:
/// the exact original request digest, unit binding, and hook kind, version and
/// payload.
fn same_request(existing: &OperationRecord, request: &BeginRequest) -> bool {
    let Some((_, _, digest, finalize)) = existing.delegation() else {
        return false;
    };
    let Some(request_digest) = request.request_digest.as_deref() else {
        return false;
    };
    let Some(request_finalize) = request.finalize.as_ref() else {
        return false;
    };
    digest == request_digest
        && finalize == request_finalize
        && existing.unit.same_binding(&request.unit)
}

/// Advance one operation to the next phase, publishing the new generation
/// durably before the effect it authorizes. Crate-private and
/// capability-checked: only this crate's checked coordinator calls it, the
/// [`IntentWrite`] token orders the write after every unit or root lock, and
/// the caller's own unit lock serializes concurrent writers of one record.
///
/// The transitions are exactly:
///
/// - one step forward on the ladder `intent` → `renamed` → `marker_last` →
///   `removed` → `deleted`, never backward and never skipping: every
///   intermediate phase is its own durable evidence (`intent` before the
///   quarantine rename, `renamed` before the first unlink, `marker_last`
///   before the leaf's removal, `removed` only after Barge's own final
///   unlink), so no publish can stand in for the evidence it skips;
/// - `deleted` → `complete`, the hook's acknowledgement;
/// - `intent` → `cancelled`, the pre-effect cancellation, retained.
///
/// A terminal record never advances again; nothing rewinds; a `renamed` or
/// later record is never cancelled, because the effect has already begun.
/// The checked coordinator of the next slice is this function's in-crate
/// caller; today the crate's own regressions drive it, so a production build
/// that names no caller yet must not fail the unused-code lint on the
/// contract's own evidence chain.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn advance_operation(
    write: &IntentWrite,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    record: &OperationRecord,
    phase: OperationPhase,
) -> Result<OperationRecord> {
    let allowed = match (record.phase, phase) {
        (OperationPhase::Deleted, OperationPhase::Complete) => true,
        (OperationPhase::Intent, OperationPhase::Cancelled) => true,
        (from, to) => match (from.ladder_position(), to.ladder_position()) {
            (Some(from), Some(to)) => to == from + 1,
            _ => false,
        },
    };
    if !allowed {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "an operation at `{}` cannot advance to `{}`",
                record.phase.as_str(),
                phase.as_str()
            ),
        )
        .with_detail("sequence", record.sequence.into()));
    }
    let mut next = record.clone();
    next.phase = phase;
    next.generation = record.generation.saturating_add(1);
    next.phase_ms = now_ms().max(record.phase_ms);
    if phase == OperationPhase::Complete {
        next.finalize_status = FinalizeStatus::Acknowledged;
    }
    if phase == OperationPhase::Cancelled {
        next.finalize_status = FinalizeStatus::Released;
    }
    let root = open_journal_root(dirs, observer, state)?;
    publish_operation(write, &root, &next)?;
    Ok(next)
}

/// The operation journal's proof-only surface: compiled only under the
/// `proof-journal` feature, which no ordinary build enables, and reached only
/// by the staged proof driver (`scripts/native/journal-proof.py`). These are
/// thin delegations to the capability-checked crate-private phase writers —
/// the [`IntentWrite`] token is still required, so an armed caller proves the
/// same ordering as any in-crate writer — and they add no rule and relax no
/// check. An ordinary release carries none of this.
#[cfg(feature = "proof-journal")]
pub mod proof {
    use super::{
        advance_operation, record_operation_captures, record_operation_foreign_capture,
        CaptureRecord, ForeignCapture, IntentWrite, OperationPhase, OperationRecord, Result,
        StatePaths, INTENT_CAPTURES,
    };
    use crate::os::{DirOps, ProcessObserver};

    /// Advance one operation to the next phase, exactly as the crate-private
    /// writer does.
    pub fn advance(
        write: &IntentWrite,
        dirs: &dyn DirOps,
        observer: &dyn ProcessObserver,
        state: &StatePaths,
        record: &OperationRecord,
        phase: OperationPhase,
    ) -> Result<OperationRecord> {
        advance_operation(write, dirs, observer, state, record, phase)
    }

    /// Record verified captures, exactly as the crate-private writer does.
    pub fn record_captures(
        write: &IntentWrite,
        dirs: &dyn DirOps,
        observer: &dyn ProcessObserver,
        state: &StatePaths,
        record: &OperationRecord,
        captures: &[CaptureRecord],
    ) -> Result<OperationRecord> {
        let bound_holds = captures.len() <= INTENT_CAPTURES;
        debug_assert!(bound_holds, "the writer enforces its own capture bound");
        record_operation_captures(write, dirs, observer, state, record, captures)
    }

    /// Record one failed capture, exactly as the crate-private writer does.
    pub fn record_foreign_capture(
        write: &IntentWrite,
        dirs: &dyn DirOps,
        observer: &dyn ProcessObserver,
        state: &StatePaths,
        record: &OperationRecord,
        foreign: &ForeignCapture,
    ) -> Result<OperationRecord> {
        record_operation_foreign_capture(write, dirs, observer, state, record, foreign)
    }
}

/// Record the captures this operation has verified and may still unlink,
/// published before each unlink, exactly as the shipped evidence chain does.
/// Crate-private and capability-checked like [`advance_operation`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn record_operation_captures(
    write: &IntentWrite,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    record: &OperationRecord,
    captures: &[CaptureRecord],
) -> Result<OperationRecord> {
    if captures.len() > INTENT_CAPTURES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!("the operation would record more than {INTENT_CAPTURES} captures"),
        )
        .with_detail("sequence", record.sequence.into()));
    }
    let mut next = record.clone();
    next.captures = captures.to_vec();
    next.generation = record.generation.saturating_add(1);
    next.phase_ms = now_ms().max(record.phase_ms);
    let root = open_journal_root(dirs, observer, state)?;
    publish_operation(write, &root, &next)?;
    Ok(next)
}

/// Record one capture whose verification failed, published before its
/// put-back, so a failed capture is never relabelled as ordinary content.
/// Crate-private and capability-checked like [`advance_operation`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn record_operation_foreign_capture(
    write: &IntentWrite,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    record: &OperationRecord,
    foreign: &ForeignCapture,
) -> Result<OperationRecord> {
    let mut next = record.clone();
    match next
        .foreign_captures
        .iter_mut()
        .find(|held| held.private == foreign.private)
    {
        // A second record of one capture is its later evidence - the put-back
        // answer - and replaces the first rather than duplicating it.
        Some(held) => *held = foreign.clone(),
        None => next.foreign_captures.push(foreign.clone()),
    }
    if next.foreign_captures.len() > INTENT_FOREIGN_CAPTURES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the operation would record more than {INTENT_FOREIGN_CAPTURES} foreign captures"
            ),
        )
        .with_detail("sequence", record.sequence.into()));
    }
    next.generation = record.generation.saturating_add(1);
    next.phase_ms = now_ms().max(record.phase_ms);
    let root = open_journal_root(dirs, observer, state)?;
    publish_operation(write, &root, &next)?;
    Ok(next)
}

/// Observe the operation journal the way a future admission consult does: one
/// immediate try-shared acquisition of the journal's lock, then one fail-closed
/// read.
///
/// - `Ok(None)` when no journal writer has ever locked: the read itself
///   decides whether the journal is unbound (`None`) or answers (`Some`).
/// - `Ok(Some(view))` while the shared lock is held, through a complete
///   fail-closed read.
/// - [`ErrorKind::Locked`] when another live owner holds the journal: the
///   consult answers unknown immediately, never blocking, and the writer
///   refuses its publication — admission never waits on the journal.
pub fn consult_operations(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    locks: &mut LockSet,
    max: usize,
) -> Result<Option<JournalView>> {
    let _observation = locks.try_observe_operations()?;
    read_operations(dirs, observer, state, max)
}

/// One record the diagnostic could read, summarized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticRecord {
    /// The record's journal sequence.
    pub sequence: u64,
    /// The phase the record reached.
    pub phase: OperationPhase,
    /// Whether the record still excludes conflicting work.
    pub binds_conflict: bool,
}

/// One finding the read-only custody diagnostic reports.
///
/// Findings describe; they never authorize repair. A gap is proven loss, a
/// broken binding is incomplete authority, and both stay blocked until an
/// operator rules on them; no command in this store invents a placeholder,
/// renumbers past unknown work, or infers a request from missing bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalFinding {
    /// The journal directory is gone, replaced or otherwise refused at the
    /// custody boundary while a binding exists.
    JournalDirectoryMissing,
    /// The journal's custody boundary refused to open: a symbolic link, a
    /// non-directory, or an unsafe mode at the state root or the journal
    /// directory's name. Nothing was followed or published into it.
    CustodyRefused {
        /// The custody refusal, bounded.
        detail: String,
    },
    /// The identity binding is missing while the epoch exists.
    IdentityMissing,
    /// The identity binding is unreadable, over-cap, or not the document the
    /// schema names.
    IdentityUnreadable {
        /// The reader's own refusal, bounded.
        detail: String,
    },
    /// The epoch document is missing while the binding exists.
    EpochMissing,
    /// The epoch document is unreadable, over-cap, or not the document the
    /// schema names.
    EpochUnreadable {
        /// The reader's own refusal, bounded.
        detail: String,
    },
    /// The binding and the epoch disagree on the creation id or time.
    IdentityMismatch {
        /// The binding's creation id.
        identity: String,
        /// The epoch's creation id.
        epoch: String,
    },
    /// Record files exist with no complete authority pair above them.
    RecordsWithoutAuthority,
    /// A sequence through the watermark has no record: proven loss.
    RecordMissing {
        /// The sequence that is gone.
        sequence: u64,
    },
    /// A record file exists but cannot be read, parsed or validated as this
    /// major, or carries a sequence that is not its file name.
    RecordUnreadable {
        /// The record's file-name sequence, when one could be classified.
        sequence: Option<u64>,
        /// The reader's own refusal, bounded.
        detail: String,
    },
    /// A file in the journal directory is neither the epoch nor a canonical
    /// numbered record.
    StrayEntry {
        /// The entry's name.
        name: String,
    },
    /// More than one unwatermarked record, or one numbered beyond watermark
    /// plus one: broken allocation, never renumbered.
    BrokenAllocation {
        /// The sequences that cannot be accounted for.
        sequences: Vec<u64>,
    },
    /// The journal holds more record files than the row cap.
    OverRowCap {
        /// How many entries were seen before the bound refused.
        rows: usize,
        /// The row cap that refused.
        cap: usize,
    },
    /// One verified allocation tail awaits reconciliation. Recoverable by any
    /// writer's next pass; reads still answer.
    TailPending {
        /// The tail's sequence.
        sequence: u64,
    },
}

impl JournalFinding {
    /// Whether the finding blocks every authoritative answer until an operator
    /// rules on it.
    pub fn blocks_reads(&self) -> bool {
        !matches!(self, Self::TailPending { .. })
    }

    /// The finding's name, as a report spells it.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::JournalDirectoryMissing => "journal_directory_missing",
            Self::CustodyRefused { .. } => "custody_refused",
            Self::IdentityMissing => "identity_missing",
            Self::IdentityUnreadable { .. } => "identity_unreadable",
            Self::EpochMissing => "epoch_missing",
            Self::EpochUnreadable { .. } => "epoch_unreadable",
            Self::IdentityMismatch { .. } => "identity_mismatch",
            Self::RecordsWithoutAuthority => "records_without_authority",
            Self::RecordMissing { .. } => "record_missing",
            Self::RecordUnreadable { .. } => "record_unreadable",
            Self::StrayEntry { .. } => "stray_entry",
            Self::BrokenAllocation { .. } => "broken_allocation",
            Self::OverRowCap { .. } => "over_row_cap",
            Self::TailPending { .. } => "tail_pending",
        }
    }
}

/// One safe, read-only custody diagnosis of the operation journal.
///
/// This is the bounded diagnostic surface the design names, to the degree
/// safely specified: it reports what exists, what is readable, and every
/// finding, and it performs no write. Resolution of a proven gap or a broken
/// binding is an explicit operator decision, out of this store's scope; the
/// diagnosis never claims to resolve, and [`Self::authoritative`] answers
/// whether a read would answer at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalDiagnostic {
    /// The identity binding as read, when present and readable.
    pub identity: Option<JournalIdentity>,
    /// The epoch document as read, when present and readable.
    pub epoch: Option<JournalEpoch>,
    /// The records that could be read and validated, in sequence order.
    pub records: Vec<DiagnosticRecord>,
    /// Every finding, including the recoverable allocation tail.
    pub findings: Vec<JournalFinding>,
}

impl JournalDiagnostic {
    /// Whether a fail-closed read of this journal would answer.
    pub fn authoritative(&self) -> bool {
        self.findings.iter().all(|finding| !finding.blocks_reads())
    }
}

/// Diagnose the operation journal's custody, read-only.
///
/// Every failure class [`read_operations`] refuses is reported here as a
/// finding instead, so an operator can see exactly what is broken — a gap's
/// sequence, a binding's absence, a stray entry's name — without the journal
/// being repaired, renumbered or recreated by the diagnosis.
pub fn inspect_operation_journal(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    max: usize,
) -> Result<JournalDiagnostic> {
    let mut findings = Vec::new();
    let root = match open_journal_root(dirs, observer, state) {
        Ok(root) => root,
        Err(error) => {
            findings.push(JournalFinding::CustodyRefused {
                detail: error.message().to_owned(),
            });
            return Ok(JournalDiagnostic {
                identity: None,
                epoch: None,
                records: Vec::new(),
                findings,
            });
        }
    };
    let identity = match root.read_identity() {
        Ok(identity) => identity,
        Err(error) => {
            findings.push(JournalFinding::IdentityUnreadable {
                detail: error.message().to_owned(),
            });
            None
        }
    };
    let epoch = match root.read_epoch() {
        Ok(epoch) => epoch,
        Err(error) => {
            findings.push(JournalFinding::EpochUnreadable {
                detail: error.message().to_owned(),
            });
            None
        }
    };
    match (&identity, &epoch) {
        (None, None) => {}
        (Some(_), None) => {
            // The binding exists; the epoch document is gone or the journal
            // directory itself is: both are the same detected class at this
            // boundary, and the finding says which side is missing.
            if root.operations.is_none() {
                findings.push(JournalFinding::JournalDirectoryMissing);
            } else {
                findings.push(JournalFinding::EpochMissing);
            }
        }
        (None, Some(_)) => findings.push(JournalFinding::IdentityMissing),
        (Some(identity), Some(epoch)) => {
            if identity.creation_id != epoch.creation_id || identity.created_ms != epoch.created_ms
            {
                findings.push(JournalFinding::IdentityMismatch {
                    identity: identity.creation_id.clone(),
                    epoch: epoch.creation_id.clone(),
                });
            }
        }
    }
    let Some(operations) = root.operations.as_ref() else {
        // Nothing at the journal directory's name: with no binding either,
        // that is the defined unbound state; with a binding it is already
        // reported above.
        return Ok(JournalDiagnostic {
            identity,
            epoch,
            records: Vec::new(),
            findings,
        });
    };
    let names = match dirs.list_names(operations, max.saturating_add(1)) {
        Ok(names) => names,
        Err(error) => {
            findings.push(JournalFinding::RecordUnreadable {
                sequence: None,
                detail: error.message().to_owned(),
            });
            return Ok(JournalDiagnostic {
                identity,
                epoch,
                records: Vec::new(),
                findings,
            });
        }
    };
    if names.truncated {
        findings.push(JournalFinding::OverRowCap {
            rows: names.total.max(names.names.len()),
            cap: max,
        });
        return Ok(JournalDiagnostic {
            identity,
            epoch,
            records: Vec::new(),
            findings,
        });
    }
    if identity.is_none()
        && epoch.is_none()
        && names
            .names
            .iter()
            .any(|name| !matches!(classify_name(name), NameClass::Epoch))
    {
        findings.push(JournalFinding::RecordsWithoutAuthority);
    }
    let mut records = Vec::new();
    let mut unreadable = false;
    for name in &names.names {
        let sequence = match classify_name(name) {
            NameClass::Epoch => continue,
            NameClass::Record(sequence) => sequence,
            NameClass::Invalid => {
                findings.push(JournalFinding::StrayEntry { name: name.clone() });
                continue;
            }
        };
        match root.read_child_document(operations, name) {
            Ok(Some(bytes)) => {
                let path = Path::new(name.as_str());
                match parse_schema::<OperationRecord>(&bytes, path, OPERATION_SCHEMA) {
                    Ok(record) if record.sequence == sequence && record.validate().is_ok() => {
                        records.push(DiagnosticRecord {
                            sequence: record.sequence,
                            phase: record.phase,
                            binds_conflict: record.binds_conflict(),
                        });
                    }
                    Ok(record) => {
                        let detail = if record.sequence != sequence {
                            format!(
                                "the record carries sequence {}, which is not its file name",
                                record.sequence
                            )
                        } else {
                            "the record does not validate".to_owned()
                        };
                        findings.push(JournalFinding::RecordUnreadable {
                            sequence: Some(sequence),
                            detail,
                        });
                        unreadable = true;
                    }
                    Err(error) => {
                        findings.push(JournalFinding::RecordUnreadable {
                            sequence: Some(sequence),
                            detail: error.message().to_owned(),
                        });
                        unreadable = true;
                    }
                }
            }
            Ok(None) => {
                findings.push(JournalFinding::RecordUnreadable {
                    sequence: Some(sequence),
                    detail: "the record vanished between its listing and its read".to_owned(),
                });
                unreadable = true;
            }
            Err(error) => {
                findings.push(JournalFinding::RecordUnreadable {
                    sequence: Some(sequence),
                    detail: error.message().to_owned(),
                });
                unreadable = true;
            }
        }
    }
    if !unreadable {
        if let Some(epoch) = &epoch {
            let watermark = epoch.watermark;
            let beyond: Vec<u64> = records
                .iter()
                .filter(|record| record.sequence > watermark.saturating_add(1))
                .map(|record| record.sequence)
                .collect();
            let tails: Vec<u64> = records
                .iter()
                .filter(|record| record.sequence == watermark.saturating_add(1))
                .map(|record| record.sequence)
                .collect();
            if tails.len() > 1 {
                let mut sequences = tails.clone();
                sequences.extend(&beyond);
                sequences.sort_unstable();
                findings.push(JournalFinding::BrokenAllocation { sequences });
            } else if let Some(sequence) = beyond.first() {
                findings.push(JournalFinding::BrokenAllocation {
                    sequences: vec![*sequence],
                });
            } else if tails.len() == 1 {
                findings.push(JournalFinding::TailPending { sequence: tails[0] });
            }
            if beyond.is_empty() && tails.len() <= 1 {
                let mut expected: u64 = 1;
                for record in &records {
                    if record.sequence > watermark {
                        break;
                    }
                    if record.sequence != expected {
                        findings.push(JournalFinding::RecordMissing { sequence: expected });
                        break;
                    }
                    expected += 1;
                }
                if expected <= watermark
                    && !findings
                        .iter()
                        .any(|finding| matches!(finding, JournalFinding::RecordMissing { .. }))
                {
                    findings.push(JournalFinding::RecordMissing { sequence: expected });
                }
            }
        }
    }
    Ok(JournalDiagnostic {
        identity,
        epoch,
        records,
        findings,
    })
}

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

    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        advance_operation, begin_operation, consult_operations, epoch_path, init_operation_journal,
        inspect_operation_journal, read_operations, record_operation_captures, record_path,
        BeginRequest, Begun, FinalizeSpec, FinalizeStatus, JournalEpoch, JournalFinding,
        JournalIdentity, OperationPhase, OperationRecord, OperationUnit, ResumePolicy,
        EPOCH_FILE_NAME,
    };
    use crate::contract::{
        INTENT_ROWS, JOURNAL_EPOCH_SCHEMA, JOURNAL_IDENTITY_SCHEMA, OPERATION_PAYLOAD_CAP_BYTES,
        OPERATION_SCHEMA,
    };
    use crate::error::{Error, ErrorKind};
    use crate::os::{platform, DirHandle, DirOps, ProcessObserver, VolumeUuid};
    use crate::state::{
        now_ms, publish_intent, read_intents, CaptureRecord, CaptureVerdict, ForeignCapture,
        IntentPhase, IntentRecord, IntentUnit, LockSet, PutBackOutcome, StatePaths,
        QUARANTINE_PREFIX,
    };
    use crate::testkit::{FakeAccount, TempDir};

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";
    const DIGEST: &str = "4d1f8d3f1d6bbbf19bb1d3ff5b6e3d5a2c4d6e7f8091a2b3c4d5e6f708192a3b";

    fn fixture(label: &str) -> (TempDir, StatePaths, VolumeUuid) {
        let dir = TempDir::new(label);
        let state = StatePaths::resolve(
            &FakeAccount {
                home: dir.path().to_path_buf(),
            },
            Some(&dir.join("state")),
        )
        .expect("the fixture state directory resolves");
        state.initialize().expect("the layout exists");
        let uuid = VolumeUuid::parse(UUID).expect("a UUID");
        (dir, state, uuid)
    }

    /// A delegated unit binding, as a caller's verified plan would present it.
    fn unit(uuid: &VolumeUuid, inode: u64) -> OperationUnit {
        OperationUnit {
            volume_uuid: uuid.clone(),
            mount_hint: PathBuf::from("/Volumes/Builds"),
            root_dir: String::new(),
            folder: "oxide/outputs".to_owned(),
            parent: String::new(),
            name: format!("{:032x}", 8),
            kind: crate::unit::UnitKind::Directory,
            device: 16_777_219,
            inode,
            marker: Some(crate::unit::UnitMarker::new(
                crate::unit::MarkerFamily::OxideOutputs,
                "a".repeat(64),
            )),
            root_id: None,
            capture_name: None,
            grant: crate::unit::UnitGrant::delegation(crate::unit::MarkerFamily::OxideOutputs),
        }
    }

    /// The finalize hook a delegated begin carries.
    fn finalize() -> FinalizeSpec {
        FinalizeSpec {
            kind: "oxide.settle-outputs".to_owned(),
            version: "1".to_owned(),
            payload: "{\"checkout\":\"c\"}".to_owned(),
        }
    }

    fn delegated_request(uuid: &VolumeUuid) -> BeginRequest {
        BeginRequest::delegated(
            "oxide.outputs",
            "prune-run-1",
            DIGEST,
            finalize(),
            unit(uuid, 4_242),
        )
    }

    fn init(state: &StatePaths) {
        let mut locks = LockSet::new(state.clone());
        init_operation_journal(&platform(), &platform(), state, &mut locks)
            .expect("the journal initializes");
    }

    fn begin(dirs: &dyn DirOps, state: &StatePaths, request: BeginRequest) -> Begun {
        let mut locks = LockSet::new(state.clone());
        begin_operation(dirs, &platform(), state, &mut locks, request)
            .expect("the operation begins")
    }

    fn begin_refused(
        dirs: &dyn DirOps,
        observer: &dyn ProcessObserver,
        state: &StatePaths,
        request: BeginRequest,
    ) -> crate::error::Error {
        let mut locks = LockSet::new(state.clone());
        begin_operation(dirs, observer, state, &mut locks, request).expect_err("the begin refuses")
    }

    /// The intent-write step, ordered after the volume lock, as the mutating
    /// paths take it.
    fn write_token(locks: &mut LockSet, uuid: &VolumeUuid) -> crate::state::IntentWrite {
        let _volume = locks.lock_volume(uuid).expect("the volume lock");
        locks.begin_intent_write().expect("the intent-write step")
    }

    #[test]
    fn explicit_initialization_publishes_one_pair_and_repeats_as_a_no_op() {
        let (_dir, state, _uuid) = fixture("operation-init");
        init(&state);
        let binding = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("an initialized journal answers");
        assert_eq!(binding.epoch.watermark, 0);
        assert_eq!(binding.epoch.creation_id, binding.identity.creation_id);
        assert_eq!(binding.epoch.created_ms, binding.identity.created_ms);
        assert!(binding.records.is_empty());
        // An explicit re-run is a no-op with the same identity.
        init(&state);
        let again = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("an initialized journal answers");
        assert_eq!(binding.identity.creation_id, again.identity.creation_id);
    }

    #[test]
    fn an_uninitialized_journal_reads_as_none_and_never_auto_initializes() {
        let (_dir, state, _uuid) = fixture("operation-unbound");
        let read = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the read answers");
        assert!(read.is_none(), "an unbound journal is a defined non-answer");
        let error = begin_refused(
            &platform(),
            &platform(),
            &state,
            delegated_request(&read_dummy_uuid()),
        );
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    fn read_dummy_uuid() -> VolumeUuid {
        VolumeUuid::parse(UUID).expect("a UUID")
    }

    #[test]
    fn initialization_finishes_only_a_genuinely_empty_epoch_only_journal() {
        let (_dir, state, _uuid) = fixture("operation-partial-init");
        // Simulate a stop between the epoch publication and the binding: the
        // epoch lands first by design, so craft exactly that state.
        let epoch = JournalEpoch::new("1234abcd".repeat(4), now_ms());
        publish_epoch_fixture(&state, &epoch);
        // A genuinely empty journal may be finished by an explicit re-run.
        init(&state);
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("the pair is complete");
        assert_eq!(view.identity.creation_id, epoch.creation_id);
        assert_eq!(view.identity.created_ms, epoch.created_ms);

        // A journal with records never finishes: the binding would be bound
        // after the fact.
        let (_dir2, state2, uuid2) = fixture("operation-partial-init-records");
        let mut locks2 = LockSet::new(state2.clone());
        let epoch2 = JournalEpoch {
            schema: JOURNAL_EPOCH_SCHEMA.to_owned(),
            creation_id: "abcd1234".repeat(4),
            created_ms: now_ms(),
            watermark: 1,
        };
        publish_epoch_fixture(&state2, &epoch2);
        write_fixture_record(&state2, &cancelled_record(&uuid2, 1), 1);
        let error = init_operation_journal(&platform(), &platform(), &state2, &mut locks2)
            .expect_err("records are never bound after the fact");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_binding_without_an_epoch_is_a_recreated_journal_never_re_initialized() {
        let (_dir, state, _uuid) = fixture("operation-binding-only");
        let mut locks = LockSet::new(state.clone());
        init(&state);
        std::fs::remove_file(state.root().join("operations").join(EPOCH_FILE_NAME))
            .expect("the fixture drops the epoch");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a destroyed journal is unknown, never empty");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let error = init_operation_journal(&platform(), &platform(), &state, &mut locks)
            .expect_err("initialization never re-creates a bound journal");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_epoch_without_a_binding_is_incomplete_authority() {
        let (_dir, state, _uuid) = fixture("operation-epoch-only");
        let epoch = JournalEpoch::new("deadbeef".repeat(4), now_ms());
        publish_epoch_fixture(&state, &epoch);
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("incomplete authority is unknown");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_disagreeing_pair_is_unknown() {
        let (_dir, state, _uuid) = fixture("operation-pair-mismatch");
        let epoch = JournalEpoch::new("11111111".repeat(4), 1);
        publish_epoch_fixture(&state, &epoch);
        let binding = JournalIdentity {
            schema: JOURNAL_IDENTITY_SCHEMA.to_owned(),
            creation_id: "22222222".repeat(4),
            created_ms: 1,
        };
        publish_identity_fixture(&state, &binding);
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("the pair disagrees");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn begin_allocates_sequence_one_at_intent_and_advances_the_watermark() {
        let (_dir, state, uuid) = fixture("operation-begin");
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let Begun::Began(record) = begun else {
            panic!("a fresh key begins");
        };
        assert_eq!(record.sequence, 1);
        assert_eq!(record.phase, OperationPhase::Intent);
        assert_eq!(record.resume, ResumePolicy::Caller);
        assert_eq!(record.finalize_status, FinalizeStatus::Owed);
        assert_eq!(
            record.unit.capture_name.as_deref(),
            Some(format!("{QUARANTINE_PREFIX}1").as_str())
        );
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.epoch.watermark, 1);
        assert_eq!(view.records.len(), 1);
        assert_eq!(view.tail_sequence, None);
        assert_eq!(view.nonterminal().count(), 1);
    }

    #[test]
    fn a_barge_owned_begin_carries_no_delegation_fields() {
        let (_dir, state, uuid) = fixture("operation-barge-owned");
        init(&state);
        let begun = begin(
            &platform(),
            &state,
            BeginRequest::barge_owned(unit(&uuid, 7)),
        );
        let record = begun.record();
        assert_eq!(record.resume, ResumePolicy::Barge);
        assert!(record.caller.is_none() && record.key.is_none());
        assert!(record.request_digest.is_none() && record.finalize.is_none());
    }

    #[test]
    fn a_barge_owned_begin_is_idempotent_on_unit_identity() {
        let (_dir, state, uuid) = fixture("operation-barge-idempotent");
        init(&state);
        let begun = begin(
            &platform(),
            &state,
            BeginRequest::barge_owned(unit(&uuid, 7)),
        );
        let first = begun.record().clone();
        // The same unit binds the same operation, whoever re-presents it.
        let again = begin(
            &platform(),
            &state,
            BeginRequest::barge_owned(unit(&uuid, 7)),
        );
        let Begun::Already(existing) = again else {
            panic!("a barge-owned repeat binds the existing operation");
        };
        assert_eq!(existing.sequence, first.sequence);
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.records.len(), 1, "no second pending record");

        // The unit is spoken for: a delegated begin for the same unit refuses.
        let mut same_unit = delegated_request(&uuid);
        same_unit.unit.inode = 7;
        let error = begin_refused(&platform(), &platform(), &state, same_unit);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
    }

    #[test]
    fn a_unit_a_pending_operation_binds_is_spoken_for() {
        let (_dir, state, uuid) = fixture("operation-unit-bound");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        // A delegated begin with a different key but the same unit refuses.
        let mut second = delegated_request(&uuid);
        second.key = Some("prune-run-2".to_owned());
        second.request_digest = Some("6".repeat(64));
        let error = begin_refused(&platform(), &platform(), &state, second);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        // A barge-owned begin for the same unit refuses too.
        let error = begin_refused(
            &platform(),
            &platform(),
            &state,
            BeginRequest::barge_owned(unit(&uuid, 4_242)),
        );
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // A different unit begins freely.
        let mut third = delegated_request(&uuid);
        third.key = Some("prune-run-2".to_owned());
        third.request_digest = Some("6".repeat(64));
        third.unit.inode = 4_243;
        let begun = begin(&platform(), &state, third);
        assert_eq!(begun.record().sequence, 2);
    }

    #[test]
    fn a_cancelled_unit_may_begin_again_but_a_pending_one_never() {
        let (_dir, state, uuid) = fixture("operation-cancelled-unit");
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let record = begun.record().clone();
        let mut write_locks = LockSet::new(state.clone());
        let write = write_token(&mut write_locks, &uuid);
        advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Cancelled,
        )
        .expect("the pre-effect cancellation retains the record");
        // A terminal record binds nothing: the same unit may begin again.
        let mut fresh = delegated_request(&uuid);
        fresh.key = Some("prune-run-2".to_owned());
        fresh.request_digest = Some("6".repeat(64));
        let begun = begin(&platform(), &state, fresh);
        assert_eq!(
            begun.record().sequence,
            2,
            "a cancelled unit frees the binding"
        );
    }

    #[test]
    fn records_without_a_complete_authority_pair_refuse_as_unknown_not_resource_bound() {
        // Records exist, no pair at all: the unbound read refuses as unknown.
        let (_dir, state, uuid) = fixture("operation-records-no-pair");
        write_fixture_record(&state, &cancelled_record(&uuid, 1), 1);
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("records without a pair are unknown");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        // Initialization refuses the same way, never with a row bound.
        let mut locks = LockSet::new(state.clone());
        let error = init_operation_journal(&platform(), &platform(), &state, &mut locks)
            .expect_err("initialization never binds over records");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        // An epoch that accounts for nothing but records that exist: the same
        // refusal, still unknown, still not a row bound.
        let (_dir2, state2, uuid2) = fixture("operation-epoch-zero-records");
        let epoch = JournalEpoch::new("fedcba98".repeat(4), now_ms());
        publish_epoch_fixture(&state2, &epoch);
        write_fixture_record(&state2, &cancelled_record(&uuid2, 1), 1);
        let mut locks2 = LockSet::new(state2.clone());
        let error = init_operation_journal(&platform(), &platform(), &state2, &mut locks2)
            .expect_err("an epoch-only journal with records never finishes");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_same_request_answers_already_and_any_mismatch_refuses() {
        let (_dir, state, uuid) = fixture("operation-idempotency");
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let first = begun.record().clone();
        // The exact same request is already bound.
        let again = begin(&platform(), &state, delegated_request(&uuid));
        let Begun::Already(existing) = again else {
            panic!("a same-key request binds the existing operation");
        };
        assert_eq!(existing.sequence, first.sequence);

        // A different digest under the same key never continues the record.
        let mut other = delegated_request(&uuid);
        other.request_digest = Some("5".repeat(64));
        let error = begin_refused(&platform(), &platform(), &state, other);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // A different unit binding under the same key refuses too.
        let mut moved = delegated_request(&uuid);
        moved.unit.inode = 9_999;
        let error = begin_refused(&platform(), &platform(), &state, moved);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // A different hook kind refuses.
        let mut hook = delegated_request(&uuid);
        hook.finalize = Some(FinalizeSpec {
            kind: "other.settle".to_owned(),
            version: "1".to_owned(),
            payload: "{}".to_owned(),
        });
        let error = begin_refused(&platform(), &platform(), &state, hook);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // A different payload refuses.
        let mut payload = delegated_request(&uuid);
        payload.finalize = Some(FinalizeSpec {
            kind: "oxide.settle-outputs".to_owned(),
            version: "1".to_owned(),
            payload: "{\"checkout\":\"other\"}".to_owned(),
        });
        let error = begin_refused(&platform(), &platform(), &state, payload);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // The record is untouched by every refusal.
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.records.len(), 1);
    }

    #[test]
    fn a_request_with_an_owed_finalize_is_bound_to_one_operation() {
        let (_dir, state, uuid) = fixture("operation-owed-digest");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        // The same digest under a different caller and key refuses while the
        // finalize is owed: one request binds one operation.
        let mut second = delegated_request(&uuid);
        second.caller = Some("other.caller".to_owned());
        second.key = Some("other-key".to_owned());
        let error = begin_refused(&platform(), &platform(), &state, second);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
    }

    #[test]
    fn a_crash_before_the_record_publish_leaves_the_journal_unchanged() {
        let (_dir, state, uuid) = fixture("operation-begin-fail");
        init(&state);
        super::fail_next_document_publish(std::io::Error::from_raw_os_error(28));
        let error = begin_refused(&platform(), &platform(), &state, delegated_request(&uuid));
        assert_eq!(error.kind(), ErrorKind::Io);
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal still reads")
            .expect("initialized");
        assert_eq!(view.epoch.watermark, 0);
        assert!(view.records.is_empty(), "nothing happened");
    }

    #[test]
    fn a_stop_between_the_record_and_the_watermark_is_one_recoverable_tail() {
        let (_dir, state, uuid) = fixture("operation-tail");
        init(&state);
        // The record publishes, then the watermark advance fails: the
        // allocation tail state.
        super::fail_next_watermark_publish(std::io::Error::from_raw_os_error(28));
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        assert!(
            matches!(begun, Begun::Began(_)),
            "the record is confirmed durable"
        );
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the tail state reads")
            .expect("initialized");
        assert_eq!(view.epoch.watermark, 0);
        assert_eq!(view.records.len(), 1);
        assert_eq!(view.tail_sequence, Some(1));
        assert_eq!(view.nonterminal().count(), 1, "the tail binds, too");

        // The next allocation reconciles the tail first, then allocates. The
        // second request is a genuinely different operation - its own unit
        // and its own request digest, because the first digest is still owed
        // and one pending operation binds one unit.
        let mut second = delegated_request(&uuid);
        second.key = Some("prune-run-2".to_owned());
        second.request_digest = Some("6".repeat(64));
        second.unit.inode = 4_243;
        let begun = begin(&platform(), &state, second);
        assert_eq!(begun.record().sequence, 2);
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.epoch.watermark, 2);
        assert_eq!(view.tail_sequence, None);
    }

    #[test]
    fn a_sequence_gap_is_proven_loss_and_never_silence() {
        let (_dir, state, uuid) = fixture("operation-gap");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        let mut second = delegated_request(&uuid);
        second.key = Some("prune-run-2".to_owned());
        second.request_digest = Some("6".repeat(64));
        second.unit.inode = 4_243;
        begin(&platform(), &state, second);
        // The first record disappears: the gap proves its loss.
        std::fs::remove_file(record_path(&state, 1)).expect("the fixture drops a record");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a gap is unknown, never a short answer");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("record 1 is missing"));
    }

    #[test]
    fn a_record_lost_after_removal_with_the_finalize_owed_stays_unknown() {
        let (_dir, state, uuid) = fixture("operation-lost-after-removed");
        let mut locks = LockSet::new(state.clone());
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let mut record = begun.record().clone();
        let write = write_token(&mut locks, &uuid);
        for phase in [
            OperationPhase::Renamed,
            OperationPhase::MarkerLast,
            OperationPhase::Removed,
            OperationPhase::Deleted,
        ] {
            record = advance_operation(&write, &platform(), &platform(), &state, &record, phase)
                .expect("the ladder advances");
        }
        assert!(record.is_pending() && record.finalization_owed());
        std::fs::remove_file(record_path(&state, 1)).expect("the fixture drops the record");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("the loss is a gap, never silence");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn broken_allocation_refuses_multi_tail_and_beyond_tail() {
        let (_dir, state, uuid) = fixture("operation-multi-tail");
        init(&state);
        // Two unwatermarked records: broken allocation, never renumbered.
        for sequence in [1_u64, 2] {
            let record = cancelled_record(&uuid, sequence);
            write_fixture_record(&state, &record, sequence);
        }
        // (Both records carry `barge.operation/1` already.)
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("two unwatermarked records refuse");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        // One record beyond watermark + 1 is equally refused.
        let (_dir2, state2, uuid2) = fixture("operation-beyond-tail");
        init(&state2);
        let record = cancelled_record(&uuid2, 2);
        write_fixture_record(&state2, &record, 2);
        let error = read_operations(&platform(), &platform(), &state2, INTENT_ROWS)
            .expect_err("a record beyond watermark + 1 refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn malformed_unknown_format_and_name_mismatched_records_refuse() {
        let (_dir, state, uuid) = fixture("operation-malformed");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        // A malformed record.
        std::fs::write(record_path(&state, 1), "{\"schema\": \"barge.operation/1\"")
            .expect("the fixture writes");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a malformed record refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        // An unknown format: the shipped intent major in the operations
        // directory is never silently read as an operation.
        let intent = legacy_intent_record(&uuid);
        let bytes = serde_json::to_vec_pretty(&intent).expect("the fixture encodes");
        std::fs::write(record_path(&state, 1), bytes).expect("the fixture writes");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a foreign format refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        // A record whose content sequence is not its file name.
        let moved = cancelled_record(&uuid, 1);
        write_fixture_record(&state, &moved, 3);
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a name mismatch refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_stray_entry_or_leading_zero_name_refuses() {
        let (_dir, state, _uuid) = fixture("operation-stray");
        init(&state);
        std::fs::write(
            state.root().join("operations").join("notes.txt"),
            "not a journal entry",
        )
        .expect("the fixture writes");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a stray entry refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        std::fs::remove_file(state.root().join("operations").join("notes.txt"))
            .expect("the fixture cleans");
        std::fs::write(state.root().join("operations").join("01.json"), "{}")
            .expect("the fixture writes");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a leading-zero name refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_over_cap_document_refuses_the_read() {
        let (_dir, state, _uuid) = fixture("operation-over-cap");
        init(&state);
        use std::os::unix::fs::PermissionsExt;
        let path = record_path(&state, 1);
        std::fs::write(&path, vec![b'a'; crate::contract::INTENT_CAP_BYTES + 1])
            .expect("the fixture writes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the fixture is private");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("an over-cap record refuses");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
    }

    #[test]
    fn the_row_bound_refuses_and_begin_refuses_an_exhausted_journal() {
        let (_dir, state, uuid) = fixture("operation-capacity");
        init(&state);
        // Retained cancelled records count toward the cap, exactly like live
        // ones: the journal is filled to its cap with retained work.
        for sequence in 1..=INTENT_ROWS as u64 {
            let mut record = cancelled_record(&uuid, sequence);
            record.schema = OPERATION_SCHEMA.to_owned();
            write_fixture_record(&state, &record, sequence);
        }
        let epoch = JournalEpoch {
            schema: JOURNAL_EPOCH_SCHEMA.to_owned(),
            creation_id: "abc12345".repeat(4),
            created_ms: now_ms(),
            watermark: INTENT_ROWS as u64,
        };
        let identity = JournalIdentity {
            schema: JOURNAL_IDENTITY_SCHEMA.to_owned(),
            creation_id: epoch.creation_id.clone(),
            created_ms: epoch.created_ms,
        };
        publish_epoch_fixture(&state, &epoch);
        publish_identity_fixture(&state, &identity);

        // The journal at its cap answers, but never allocates again.
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the full journal answers")
            .expect("initialized");
        assert_eq!(view.records.len(), INTENT_ROWS);
        // The refusal is the NEW-allocation gate: a genuinely fresh request
        // refuses, without a write.
        let mut fresh = delegated_request(&uuid);
        fresh.key = Some("prune-run-fresh".to_owned());
        fresh.request_digest = Some("8".repeat(64));
        fresh.unit.inode = 8_001;
        let error = begin_refused(&platform(), &platform(), &state, fresh);
        assert_eq!(error.kind(), ErrorKind::ResourceBound);

        // A read under the cap refuses rather than truncating.
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS - 1)
            .expect_err("a short read is refused, never silent");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
    }

    #[test]
    fn the_ladder_advances_forward_only_and_terminals_are_frozen() {
        let (_dir, state, uuid) = fixture("operation-ladder");
        let mut locks = LockSet::new(state.clone());
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let mut record = begun.record().clone();
        let write = write_token(&mut locks, &uuid);

        // A rewind refuses before anything is written.
        let error = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Intent,
        )
        .expect_err("the ladder never rewinds");
        assert_eq!(error.kind(), ErrorKind::Usage);

        // Skipping is refused: every phase is its own durable evidence, and a
        // publish that skipped one would stand in for the evidence it skipped.
        let error = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::MarkerLast,
        )
        .expect_err("the ladder never skips marker_last");
        assert_eq!(error.kind(), ErrorKind::Usage);
        let error = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Deleted,
        )
        .expect_err("the ladder never skips to deleted without a durable removed");
        assert_eq!(error.kind(), ErrorKind::Usage);

        // Skipping forward is one publication per step, with the generation
        // increasing.
        for phase in [
            OperationPhase::Renamed,
            OperationPhase::MarkerLast,
            OperationPhase::Removed,
            OperationPhase::Deleted,
        ] {
            record = advance_operation(&write, &platform(), &platform(), &state, &record, phase)
                .expect("the ladder advances");
            let read = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
                .expect("the journal reads")
                .expect("initialized");
            assert_eq!(
                read.records[0].phase, phase,
                "the phase is durable before its effect"
            );
        }
        assert!(record.finalization_owed(), "deleted is still owed");

        // A deleted record cannot be cancelled: the effect already began.
        let error = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Cancelled,
        )
        .expect_err("a post-effect record is never cancelled");
        assert_eq!(error.kind(), ErrorKind::Usage);

        // The ack settles the record; it is then frozen.
        record = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Complete,
        )
        .expect("the ack settles");
        assert_eq!(record.finalize_status, FinalizeStatus::Acknowledged);
        assert!(record.is_terminal());
        assert!(!record.binds_conflict());
        let error = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Deleted,
        )
        .expect_err("a settled record never advances");
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn cancellation_is_a_retained_pre_effect_transition() {
        let (_dir, state, uuid) = fixture("operation-cancel");
        let mut locks = LockSet::new(state.clone());
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let record = begun.record().clone();
        let write = write_token(&mut locks, &uuid);
        let cancelled = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            OperationPhase::Cancelled,
        )
        .expect("the pre-effect cancellation retains the record");
        assert_eq!(cancelled.finalize_status, FinalizeStatus::Released);
        assert!(cancelled.is_terminal());
        assert!(!cancelled.binds_conflict());

        // The record is retained: the gapless inventory is unchanged.
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.records.len(), 1);
        assert_eq!(view.records[0].phase, OperationPhase::Cancelled);

        // The spent key refuses; the digest no longer binds; a fresh key with
        // the same digest begins again.
        let error = begin_refused(&platform(), &platform(), &state, delegated_request(&uuid));
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        let mut fresh = delegated_request(&uuid);
        fresh.key = Some("prune-run-2".to_owned());
        let begun = begin(&platform(), &state, fresh);
        let Begun::Began(record) = begun else {
            panic!("a fresh key begins after a cancellation");
        };
        assert_eq!(record.sequence, 2);

        // A cancelled record never advances again.
        let error = advance_operation(
            &write,
            &platform(),
            &platform(),
            &state,
            &cancelled,
            OperationPhase::Renamed,
        )
        .expect_err("a terminal record is frozen");
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn capture_evidence_publishes_before_its_unlink_and_stays_bounded() {
        let (_dir, state, uuid) = fixture("operation-captures");
        let mut locks = LockSet::new(state.clone());
        init(&state);
        let begun = begin(&platform(), &state, delegated_request(&uuid));
        let record = begun.record().clone();
        let write = write_token(&mut locks, &uuid);
        let capture = CaptureRecord {
            private: format!("{}{}", crate::state::CAPTURE_PREFIX, "c".repeat(32)),
            original: "child.txt".to_owned(),
            device: record.unit.device,
            inode: 77,
            kind: "file".to_owned(),
            verdict: CaptureVerdict::Observed,
            mode: Some(0o100_644),
        };
        let next = record_operation_captures(
            &write,
            &platform(),
            &platform(),
            &state,
            &record,
            std::slice::from_ref(&capture),
        )
        .expect("the capture chain publishes");
        assert_eq!(next.generation, record.generation + 1);
        assert_eq!(next.phase, record.phase);
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.records[0].captures.len(), 1);

        let foreign = ForeignCapture {
            private: capture.private.clone(),
            original: "child.txt".to_owned(),
            device: 1,
            inode: 78,
            kind: "file".to_owned(),
            captured: None,
            put_back: PutBackOutcome::Pending,
            reason: "the captured object was not the observed one".to_owned(),
        };
        let next = super::record_operation_foreign_capture(
            &write,
            &platform(),
            &platform(),
            &state,
            &next,
            &foreign,
        )
        .expect("the foreign capture publishes before its put-back");
        assert_eq!(next.foreign_captures.len(), 1);

        // The bounds are the shipped ones.
        let many: Vec<CaptureRecord> = (0..21).map(|_| capture.clone()).collect();
        let error =
            record_operation_captures(&write, &platform(), &platform(), &state, &record, &many)
                .expect_err("the capture bound refuses");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
    }

    #[test]
    fn the_consult_refuses_immediately_while_the_journal_is_locked() {
        let (_dir, state, uuid) = fixture("operation-consult");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));

        // Another live owner holds the journal: the consult gives up instead
        // of waiting.
        let held_state = state.clone();
        let (ready, started) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let mut locks = LockSet::new(held_state);
            let _lock = locks.lock_operations().expect("the holder locks");
            ready.send(()).expect("the holder signals");
            std::thread::sleep(Duration::from_millis(300));
        });
        started
            .recv()
            .expect("the holder took the lock before the consult");
        let mut busy_locks = LockSet::new(state.clone());
        let error = consult_operations(
            &platform(),
            &platform(),
            &state,
            &mut busy_locks,
            INTENT_ROWS,
        )
        .expect_err("a busy journal is refused immediately");
        drop(busy_locks);
        assert_eq!(error.kind(), ErrorKind::Locked);
        holder.join().expect("the holder finishes");

        // With the journal free, the consult answers the authoritative read.
        let mut consult_locks = LockSet::new(state.clone());
        let view = consult_operations(
            &platform(),
            &platform(),
            &state,
            &mut consult_locks,
            INTENT_ROWS,
        )
        .expect("the consult answers when free")
        .expect("the journal is bound");
        assert_eq!(view.records.len(), 1);
    }

    #[test]
    fn the_consult_on_a_never_locked_journal_answers_unbound() {
        let (_dir, state, _uuid) = fixture("operation-consult-unbound");
        let mut locks = LockSet::new(state.clone());
        // The layout exists but the journal was never initialized and no
        // writer ever locked: no lock file exists.
        assert!(!state.operations_lock_path().exists());
        let consulted =
            consult_operations(&platform(), &platform(), &state, &mut locks, INTENT_ROWS)
                .expect("the consult answers");
        assert!(
            consulted.is_none(),
            "an unbound journal is a defined non-answer"
        );
    }

    #[test]
    fn a_non_private_or_linked_state_document_is_never_followed() {
        let (_dir, state, _uuid) = fixture("operation-private");
        init(&state);
        let epoch = state.root().join("operations").join(EPOCH_FILE_NAME);

        // A mode that opens the document to others is not the state Barge
        // published.
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&epoch)
            .expect("the epoch exists")
            .permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&epoch, permissions).expect("the fixture widens the mode");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("an exposed document refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let mut permissions = std::fs::metadata(&epoch)
            .expect("the epoch exists")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&epoch, permissions).expect("the fixture restores the mode");

        // A symbolic link at the document's name is never followed.
        let elsewhere = _dir.path().join("elsewhere.json");
        std::fs::write(&elsewhere, "{}").expect("the fixture writes a target");
        std::fs::remove_file(&epoch).expect("the fixture drops the epoch");
        std::os::unix::fs::symlink(&elsewhere, &epoch).expect("the fixture links");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a linked document refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_two_journals_coexist_and_never_mix() {
        let (_dir, state, uuid) = fixture("operation-coexistence");
        let mut locks = LockSet::new(state.clone());
        // The shipped journal keeps its own record, under its own rules.
        let intent = legacy_intent_record(&uuid);
        let write = write_token(&mut locks, &uuid);
        publish_intent(&write, &state, &intent).expect("the shipped journal writes");
        // The successor journal initializes and allocates beside it.
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        let intents = read_intents(&state, INTENT_ROWS).expect("the shipped journal reads");
        assert_eq!(intents.records.len(), 1);
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the successor journal reads")
            .expect("initialized");
        assert_eq!(view.records.len(), 1);
    }

    #[test]
    fn the_diagnostic_reports_findings_without_repairing() {
        let (_dir, state, uuid) = fixture("operation-diagnostic");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));

        // A healthy journal is authoritative, and the tail is reported as
        // recoverable when the watermark lags.
        let report = inspect_operation_journal(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the diagnosis reads");
        assert!(report.authoritative());
        assert!(report.findings.is_empty());
        assert_eq!(report.records.len(), 1);

        // A proven gap is a blocking finding, and the diagnosis changes
        // nothing.
        std::fs::remove_file(record_path(&state, 1)).expect("the fixture drops the record");
        let before: Vec<PathBuf> = std::fs::read_dir(state.root().join("operations"))
            .expect("the listing")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        let report = inspect_operation_journal(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the diagnosis reads");
        assert!(!report.authoritative());
        assert!(report
            .findings
            .iter()
            .any(|finding| matches!(finding, JournalFinding::RecordMissing { sequence: 1 })));
        let after: Vec<PathBuf> = std::fs::read_dir(state.root().join("operations"))
            .expect("the listing")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        assert_eq!(before, after, "the diagnosis performs no write");

        // An unbound journal reports as not authoritative for delegation, and
        // an over-cap journal names its bound.
        let (_dir2, state2, _uuid2) = fixture("operation-diagnostic-unbound");
        let report2 = inspect_operation_journal(&platform(), &platform(), &state2, INTENT_ROWS)
            .expect("the diagnosis reads");
        assert!(report2.findings.is_empty());
        let (_dir3, state3, uuid3) = fixture("operation-diagnostic-tail");
        init(&state3);
        super::fail_next_watermark_publish(std::io::Error::from_raw_os_error(28));
        begin(&platform(), &state3, delegated_request(&uuid3));
        let report3 = inspect_operation_journal(&platform(), &platform(), &state3, INTENT_ROWS)
            .expect("the diagnosis reads");
        assert!(
            report3.authoritative(),
            "a recoverable tail does not block reads"
        );
        assert!(report3
            .findings
            .iter()
            .any(|finding| matches!(finding, JournalFinding::TailPending { sequence: 1 })));
    }

    #[test]
    fn a_request_never_accepts_a_payload_over_its_cap_or_a_pre_set_capture_name() {
        let (_dir, state, uuid) = fixture("operation-request-bounds");
        init(&state);
        let mut over = delegated_request(&uuid);
        over.finalize = Some(FinalizeSpec {
            kind: "oxide.settle-outputs".to_owned(),
            version: "1".to_owned(),
            payload: "x".repeat(OPERATION_PAYLOAD_CAP_BYTES + 1),
        });
        let error = begin_refused(&platform(), &platform(), &state, over);
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
        let mut preset = delegated_request(&uuid);
        preset.unit.capture_name = Some(format!("{QUARANTINE_PREFIX}1"));
        let error = begin_refused(&platform(), &platform(), &state, preset);
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn the_record_validator_refuses_a_capture_name_from_another_operation() {
        let (_dir, state, uuid) = fixture("operation-capture-name");
        init(&state);
        let mut request = delegated_request(&uuid);
        request.unit.name = format!("{:032x}", 9);
        let begun = begin(&platform(), &state, request);
        let mut record = begun.record().clone();
        record.unit.capture_name = Some(format!("{QUARANTINE_PREFIX}99"));
        let error = record
            .validate()
            .expect_err("another operation's private name refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn idempotency_precedes_the_capacity_refusal_at_a_valid_full_journal() {
        let (_dir, state, uuid) = fixture("operation-capacity-idempotent");
        // A valid full journal: retained cancellations, one pending
        // Barge-owned record, one pending delegated record, watermark at cap.
        for sequence in 1..=(INTENT_ROWS - 2) as u64 {
            let record = cancelled_record(&uuid, sequence);
            write_fixture_record(&state, &record, sequence);
        }
        let barge_pending = barge_pending_record(&uuid, (INTENT_ROWS - 1) as u64, 7_001);
        write_fixture_record(&state, &barge_pending, (INTENT_ROWS - 1) as u64);
        let live =
            delegated_pending_record(&uuid, INTENT_ROWS as u64, "prune-run-live", DIGEST, 7_002);
        write_fixture_record(&state, &live, INTENT_ROWS as u64);
        let epoch = JournalEpoch {
            schema: JOURNAL_EPOCH_SCHEMA.to_owned(),
            creation_id: "abc12345".repeat(4),
            created_ms: now_ms(),
            watermark: INTENT_ROWS as u64,
        };
        publish_epoch_fixture(&state, &epoch);
        publish_identity_fixture(
            &state,
            &JournalIdentity {
                schema: JOURNAL_IDENTITY_SCHEMA.to_owned(),
                creation_id: epoch.creation_id.clone(),
                created_ms: epoch.created_ms,
            },
        );
        let before = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the full journal answers")
            .expect("initialized");

        // The exact live request is already bound, at capacity.
        let mut live = delegated_request(&uuid);
        live.key = Some("prune-run-live".to_owned());
        live.unit.inode = 7_002;
        let begun = begin(&platform(), &state, live);
        let Begun::Already(existing) = begun else {
            panic!("the live request is already bound at capacity");
        };
        assert_eq!(existing.sequence, INTENT_ROWS as u64);
        assert_eq!(existing.key.as_deref(), Some("prune-run-live"));

        // The bound Barge-owned unit is idempotent at capacity too.
        let begun = begin(
            &platform(),
            &state,
            BeginRequest::barge_owned(unit(&uuid, 7_001)),
        );
        let Begun::Already(existing) = begun else {
            panic!("the bound unit is already bound at capacity");
        };
        assert_eq!(existing.sequence, (INTENT_ROWS - 1) as u64);

        // A mismatched request under the live key refuses as itself.
        let mut mismatched = delegated_request(&uuid);
        mismatched.key = Some("prune-run-live".to_owned());
        mismatched.request_digest = Some("7".repeat(64));
        let error = begin_refused(&platform(), &platform(), &state, mismatched);
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // A genuinely new allocation refuses, and writes nothing.
        let mut fresh = delegated_request(&uuid);
        fresh.key = Some("prune-run-fresh".to_owned());
        fresh.request_digest = Some("8".repeat(64));
        fresh.unit.inode = 8_001;
        let error = begin_refused(&platform(), &platform(), &state, fresh);
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
        let after = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal still answers")
            .expect("initialized");
        assert_eq!(after.records.len(), before.records.len());
        assert_eq!(after.epoch.watermark, before.epoch.watermark);
        assert_eq!(before.records.len(), INTENT_ROWS);
    }

    #[test]
    fn a_replaced_journal_directory_is_never_followed() {
        let (_dir, state, uuid) = fixture("operation-linked-dir");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        // The journal directory's name becomes a link to an empty substitute.
        let substitute = state.root().join("substitute");
        std::fs::create_dir(&substitute).expect("the fixture creates the substitute");
        let original = state.root().join("operations-original");
        std::fs::rename(state.operations_dir(), &original).expect("the fixture moves");
        std::os::unix::fs::symlink(&substitute, state.operations_dir()).expect("the fixture links");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("a linked journal directory refuses reads");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let error = begin_refused(&platform(), &platform(), &state, delegated_request(&uuid));
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let mut locks = LockSet::new(state.clone());
        let error = init_operation_journal(&platform(), &platform(), &state, &mut locks)
            .expect_err("a linked journal directory refuses init");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let entries = std::fs::read_dir(&substitute)
            .expect("the substitute lists")
            .count();
        assert_eq!(entries, 0, "nothing was published into the substitute");
        // The real directory still answers through its own name.
        std::fs::remove_file(state.operations_dir()).expect("the fixture drops the link");
        std::fs::rename(&original, state.operations_dir()).expect("the fixture restores");
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the restored journal reads")
            .expect("initialized");
        assert_eq!(view.records.len(), 1);
    }

    #[test]
    fn an_unsafe_journal_directory_mode_refuses_every_access() {
        let (_dir, state, uuid) = fixture("operation-dir-mode");
        init(&state);
        let mut permissions = std::fs::metadata(state.operations_dir())
            .expect("the journal directory exists")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(state.operations_dir(), permissions)
            .expect("the fixture widens the mode");
        let error = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect_err("an exposed journal directory refuses reads");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let error = begin_refused(&platform(), &platform(), &state, delegated_request(&uuid));
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_missing_journal_directory_reads_unbound_and_refuses_writes() {
        let (_dir, state, _uuid) = fixture("operation-missing-dir");
        std::fs::remove_dir(state.operations_dir()).expect("the fixture drops the directory");
        let read = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the read answers");
        assert!(read.is_none(), "no journal directory is the unbound state");
        let error = begin_refused(
            &platform(),
            &platform(),
            &state,
            delegated_request(&read_dummy_uuid()),
        );
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let mut locks = LockSet::new(state.clone());
        let error = init_operation_journal(&platform(), &platform(), &state, &mut locks)
            .expect_err("initialization never creates a directory");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    /// A platform seam whose every directory and document fact reports one
    /// foreign owner: the injected substitute for another account's files, so
    /// no host chown or privilege experiment is ever run.
    struct ForeignOwned {
        inner: crate::os::MacOs,
        foreign_uid: u32,
    }

    impl DirOps for ForeignOwned {
        fn open_path(&self, path: &Path) -> crate::error::Result<crate::os::DirHandle> {
            self.inner.open_path(path)
        }
        fn path_facts(&self, path: &Path) -> crate::error::Result<Option<crate::os::EntryFacts>> {
            Ok(self.inner.path_facts(path)?.map(|mut facts| {
                facts.uid = self.foreign_uid;
                facts
            }))
        }
        fn open_child_dir(
            &self,
            parent: &DirHandle,
            name: &str,
        ) -> crate::error::Result<DirHandle> {
            self.inner.open_child_dir(parent, name)
        }
        fn duplicate_dir(&self, handle: &DirHandle) -> crate::error::Result<DirHandle> {
            self.inner.duplicate_dir(handle)
        }
        fn child_facts(
            &self,
            parent: &DirHandle,
            name: &str,
        ) -> crate::error::Result<Option<crate::os::EntryFacts>> {
            Ok(self.inner.child_facts(parent, name)?.map(|mut facts| {
                facts.uid = self.foreign_uid;
                facts
            }))
        }
        fn create_child_dir(
            &self,
            parent: &DirHandle,
            name: &str,
            mode: u32,
        ) -> crate::error::Result<crate::os::DirId> {
            self.inner.create_child_dir(parent, name, mode)
        }
        fn publish_child_file(
            &self,
            parent: &DirHandle,
            name: &str,
            bytes: &[u8],
            mode: u32,
        ) -> crate::error::Result<()> {
            self.inner.publish_child_file(parent, name, bytes, mode)
        }
        fn read_child_file(
            &self,
            parent: &DirHandle,
            name: &str,
            cap: usize,
        ) -> crate::error::Result<Option<Vec<u8>>> {
            self.inner.read_child_file(parent, name, cap)
        }
        fn list_children(
            &self,
            parent: &DirHandle,
            max_names: usize,
        ) -> crate::error::Result<crate::os::ChildListing> {
            self.inner.list_children(parent, max_names)
        }
        fn list_names(
            &self,
            parent: &DirHandle,
            max_names: usize,
        ) -> crate::error::Result<crate::os::DirNames> {
            self.inner.list_names(parent, max_names)
        }
        fn remove_child(
            &self,
            parent: &DirHandle,
            name: &str,
            kind: crate::os::RemoveKind,
        ) -> crate::error::Result<crate::os::ChildRemoval> {
            self.inner.remove_child(parent, name, kind)
        }
        fn rename_child(
            &self,
            parent: &DirHandle,
            from: &str,
            to: &str,
        ) -> crate::error::Result<crate::os::RenameOutcome> {
            self.inner.rename_child(parent, from, to)
        }
        fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> crate::error::Result<()> {
            self.inner.set_dir_mode(handle, mode)
        }
        fn dir_facts(&self, handle: &DirHandle) -> crate::error::Result<crate::os::EntryFacts> {
            let mut facts = self.inner.dir_facts(handle)?;
            facts.uid = self.foreign_uid;
            Ok(facts)
        }
        fn recheck_dir(&self, handle: &DirHandle) -> crate::error::Result<crate::os::DirId> {
            self.inner.recheck_dir(handle)
        }
        fn create_child_symlink(
            &self,
            parent: &DirHandle,
            name: &str,
            target: &Path,
        ) -> crate::error::Result<()> {
            self.inner.create_child_symlink(parent, name, target)
        }
        fn read_link(&self, path: &Path) -> crate::error::Result<Option<PathBuf>> {
            self.inner.read_link(path)
        }
        fn random_bytes(&self, length: usize) -> crate::error::Result<Vec<u8>> {
            self.inner.random_bytes(length)
        }
    }

    /// An observer that cannot decide the executing account at all.
    struct UndecidedAccount;

    impl crate::os::ProcessObserver for UndecidedAccount {
        fn effective_uid(&self) -> crate::error::Result<u32> {
            Err(Error::new(
                ErrorKind::Io,
                "the injected account fact is unreadable",
            ))
        }
        fn boot_identity(&self) -> crate::error::Result<String> {
            Ok("fixture-boot".to_owned())
        }
        fn process_start(&self, _pid: u32) -> crate::error::Result<Option<u64>> {
            Ok(None)
        }
        fn verify_platform_binary(
            &self,
            _classified: crate::os::ProcessIdentity,
        ) -> crate::error::Result<crate::os::PlatformStatus> {
            Err(Error::new(ErrorKind::Io, "unused in this fixture"))
        }
        fn scan_open_descriptors(
            &self,
            _request: &crate::os::DescriptorScanRequest<'_>,
        ) -> crate::error::Result<crate::os::DescriptorScan> {
            Err(Error::new(ErrorKind::Io, "unused in this fixture"))
        }
    }

    /// An observer that names one foreign account as this one's.
    struct ForeignAccount;

    impl crate::os::ProcessObserver for ForeignAccount {
        fn effective_uid(&self) -> crate::error::Result<u32> {
            Ok(4_242_424)
        }
        fn boot_identity(&self) -> crate::error::Result<String> {
            Ok("fixture-boot".to_owned())
        }
        fn process_start(&self, _pid: u32) -> crate::error::Result<Option<u64>> {
            Ok(None)
        }
        fn verify_platform_binary(
            &self,
            _classified: crate::os::ProcessIdentity,
        ) -> crate::error::Result<crate::os::PlatformStatus> {
            Err(Error::new(ErrorKind::Io, "unused in this fixture"))
        }
        fn scan_open_descriptors(
            &self,
            _request: &crate::os::DescriptorScanRequest<'_>,
        ) -> crate::error::Result<crate::os::DescriptorScan> {
            Err(Error::new(ErrorKind::Io, "unused in this fixture"))
        }
    }

    #[test]
    fn a_foreign_owned_journal_refuses_as_someone_elses_data() {
        let (_dir, state, uuid) = fixture("operation-foreign-owner");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        // Every fact the custody opens reports another account's ownership:
        // the state directory, the journal directory, and every leaf.
        let foreign = ForeignOwned {
            inner: platform(),
            foreign_uid: 4_242_424,
        };
        let error = read_operations(&foreign, &platform(), &state, INTENT_ROWS)
            .expect_err("a foreign-owned journal is never read");
        assert_eq!(error.kind(), ErrorKind::ForeignOwner);
        let error = begin_refused(&foreign, &platform(), &state, delegated_request(&uuid));
        assert_eq!(error.kind(), ErrorKind::ForeignOwner);
        let mut locks = LockSet::new(state.clone());
        let error = init_operation_journal(&foreign, &platform(), &state, &mut locks)
            .expect_err("a foreign-owned journal is never re-initialized");
        assert_eq!(error.kind(), ErrorKind::ForeignOwner);
        // The real account still reads its own journal untouched.
        let view = read_operations(&platform(), &platform(), &state, INTENT_ROWS)
            .expect("the journal reads")
            .expect("initialized");
        assert_eq!(view.records.len(), 1);

        // The mirror case: our own files, but the executing account names
        // itself as someone else — the same refusal, from the account side.
        let error = read_operations(&platform(), &ForeignAccount, &state, INTENT_ROWS)
            .expect_err("a journal is never read for another account");
        assert_eq!(error.kind(), ErrorKind::ForeignOwner);
    }

    #[test]
    fn a_foreign_owned_document_leaf_refuses_even_under_a_verified_parent() {
        let (_dir, state, uuid) = fixture("operation-foreign-leaf");
        init(&state);
        begin(&platform(), &state, delegated_request(&uuid));
        // The directories verify as this account's, but the epoch leaf is
        // another account's: a verified parent never proves a child's owner.
        struct ForeignLeaf {
            inner: crate::os::MacOs,
        }
        impl crate::os::DirOps for ForeignLeaf {
            fn open_path(&self, path: &Path) -> crate::error::Result<crate::os::DirHandle> {
                self.inner.open_path(path)
            }
            fn path_facts(
                &self,
                path: &Path,
            ) -> crate::error::Result<Option<crate::os::EntryFacts>> {
                self.inner.path_facts(path)
            }
            fn open_child_dir(
                &self,
                parent: &DirHandle,
                name: &str,
            ) -> crate::error::Result<DirHandle> {
                self.inner.open_child_dir(parent, name)
            }
            fn child_facts(
                &self,
                parent: &DirHandle,
                name: &str,
            ) -> crate::error::Result<Option<crate::os::EntryFacts>> {
                Ok(self.inner.child_facts(parent, name)?.map(|mut facts| {
                    if name == EPOCH_FILE_NAME {
                        facts.uid = 4_242_424;
                    }
                    facts
                }))
            }
            fn duplicate_dir(&self, handle: &DirHandle) -> crate::error::Result<DirHandle> {
                self.inner.duplicate_dir(handle)
            }
            fn create_child_dir(
                &self,
                parent: &DirHandle,
                name: &str,
                mode: u32,
            ) -> crate::error::Result<crate::os::DirId> {
                self.inner.create_child_dir(parent, name, mode)
            }
            fn publish_child_file(
                &self,
                parent: &DirHandle,
                name: &str,
                bytes: &[u8],
                mode: u32,
            ) -> crate::error::Result<()> {
                self.inner.publish_child_file(parent, name, bytes, mode)
            }
            fn read_child_file(
                &self,
                parent: &DirHandle,
                name: &str,
                cap: usize,
            ) -> crate::error::Result<Option<Vec<u8>>> {
                self.inner.read_child_file(parent, name, cap)
            }
            fn list_children(
                &self,
                parent: &DirHandle,
                max_names: usize,
            ) -> crate::error::Result<crate::os::ChildListing> {
                self.inner.list_children(parent, max_names)
            }
            fn list_names(
                &self,
                parent: &DirHandle,
                max_names: usize,
            ) -> crate::error::Result<crate::os::DirNames> {
                self.inner.list_names(parent, max_names)
            }
            fn remove_child(
                &self,
                parent: &DirHandle,
                name: &str,
                kind: crate::os::RemoveKind,
            ) -> crate::error::Result<crate::os::ChildRemoval> {
                self.inner.remove_child(parent, name, kind)
            }
            fn rename_child(
                &self,
                parent: &DirHandle,
                from: &str,
                to: &str,
            ) -> crate::error::Result<crate::os::RenameOutcome> {
                self.inner.rename_child(parent, from, to)
            }
            fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> crate::error::Result<()> {
                self.inner.set_dir_mode(handle, mode)
            }
            fn dir_facts(&self, handle: &DirHandle) -> crate::error::Result<crate::os::EntryFacts> {
                self.inner.dir_facts(handle)
            }
            fn recheck_dir(&self, handle: &DirHandle) -> crate::error::Result<crate::os::DirId> {
                self.inner.recheck_dir(handle)
            }
            fn create_child_symlink(
                &self,
                parent: &DirHandle,
                name: &str,
                target: &Path,
            ) -> crate::error::Result<()> {
                self.inner.create_child_symlink(parent, name, target)
            }
            fn read_link(&self, path: &Path) -> crate::error::Result<Option<PathBuf>> {
                self.inner.read_link(path)
            }
            fn random_bytes(&self, length: usize) -> crate::error::Result<Vec<u8>> {
                self.inner.random_bytes(length)
            }
        }
        let leaf = ForeignLeaf { inner: platform() };
        let error = read_operations(&leaf, &platform(), &state, INTENT_ROWS)
            .expect_err("a foreign-owned leaf is never read");
        assert_eq!(error.kind(), ErrorKind::ForeignOwner);
    }

    #[test]
    fn an_undecided_current_account_refuses_every_access() {
        let (_dir, state, _uuid) = fixture("operation-undecided-account");
        init(&state);
        let undecided = UndecidedAccount;
        let error = read_operations(&platform(), &undecided, &state, INTENT_ROWS)
            .expect_err("an undecided account never matches");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("cannot be decided"));
        let error = begin_refused(
            &platform(),
            &undecided,
            &state,
            delegated_request(&read_dummy_uuid()),
        );
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let mut locks = LockSet::new(state.clone());
        let error = init_operation_journal(&platform(), &undecided, &state, &mut locks)
            .expect_err("initialization refuses an undecided account");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    /// A pending delegated record fixture, as the capacity cases need.
    fn delegated_pending_record(
        uuid: &VolumeUuid,
        sequence: u64,
        key: &str,
        digest: &str,
        inode: u64,
    ) -> OperationRecord {
        let mut bound = unit(uuid, inode);
        bound.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
        OperationRecord {
            schema: OPERATION_SCHEMA.to_owned(),
            sequence,
            recorded_ms: now_ms(),
            phase_ms: now_ms(),
            generation: 1,
            resume: ResumePolicy::Caller,
            caller: Some("oxide.outputs".to_owned()),
            key: Some(key.to_owned()),
            request_digest: Some(digest.to_owned()),
            finalize: Some(finalize()),
            finalize_status: FinalizeStatus::Owed,
            unit: bound,
            phase: OperationPhase::Intent,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
        }
    }

    /// A pending Barge-owned record fixture, as the capacity cases need.
    fn barge_pending_record(uuid: &VolumeUuid, sequence: u64, inode: u64) -> OperationRecord {
        let mut bound = unit(uuid, inode);
        bound.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
        OperationRecord {
            schema: OPERATION_SCHEMA.to_owned(),
            sequence,
            recorded_ms: now_ms(),
            phase_ms: now_ms(),
            generation: 1,
            resume: ResumePolicy::Barge,
            caller: None,
            key: None,
            request_digest: None,
            finalize: None,
            finalize_status: FinalizeStatus::Owed,
            unit: bound,
            phase: OperationPhase::Intent,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
        }
    }

    /// A retained cancelled record, as a fixture crafts it.
    fn cancelled_record(uuid: &VolumeUuid, sequence: u64) -> OperationRecord {
        let mut unit = unit(uuid, 4_242);
        unit.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
        OperationRecord {
            schema: OPERATION_SCHEMA.to_owned(),
            sequence,
            recorded_ms: now_ms(),
            phase_ms: now_ms(),
            generation: 2,
            resume: ResumePolicy::Caller,
            caller: Some("oxide.outputs".to_owned()),
            key: Some(format!("prune-run-{sequence}")),
            request_digest: Some(DIGEST.to_owned()),
            finalize: Some(finalize()),
            finalize_status: FinalizeStatus::Released,
            unit,
            phase: OperationPhase::Cancelled,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
        }
    }

    /// Write one crafted record fixture, bypassing the store's allocation,
    /// with the private mode the reader requires.
    fn write_fixture_record(state: &StatePaths, record: &OperationRecord, sequence: u64) {
        use std::os::unix::fs::PermissionsExt;
        record.validate().expect("the fixture record is valid");
        let bytes = serde_json::to_vec_pretty(record).expect("the fixture encodes");
        let path = record_path(state, sequence);
        std::fs::write(&path, bytes).expect("the fixture writes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the fixture is private");
    }

    /// Publish a crafted epoch fixture at the epoch path, privately.
    fn publish_epoch_fixture(state: &StatePaths, epoch: &JournalEpoch) {
        use std::os::unix::fs::PermissionsExt;
        epoch.validate().expect("the fixture epoch is valid");
        let bytes = serde_json::to_vec_pretty(epoch).expect("the fixture encodes");
        let path = epoch_path(state);
        std::fs::write(&path, bytes).expect("the fixture writes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the fixture is private");
    }

    /// Publish a crafted identity fixture at the binding path, privately.
    fn publish_identity_fixture(state: &StatePaths, identity: &JournalIdentity) {
        use std::os::unix::fs::PermissionsExt;
        identity.validate().expect("the fixture binding is valid");
        let bytes = serde_json::to_vec_pretty(identity).expect("the fixture encodes");
        let path = state.journal_identity_path();
        std::fs::write(&path, bytes).expect("the fixture writes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the fixture is private");
    }

    /// The shipped `barge.intent/2` record, for the coexistence fixtures.
    fn legacy_intent_record(uuid: &VolumeUuid) -> IntentRecord {
        let intent_id = format!("{:032x}", 7);
        let now = now_ms();
        IntentRecord {
            schema: crate::contract::INTENT_SCHEMA.to_owned(),
            intent_id: intent_id.clone(),
            volume_uuid: uuid.clone(),
            mount_hint: PathBuf::from("/Volumes/Builds"),
            root_dir: String::new(),
            folder: "example/pool".to_owned(),
            unit: IntentUnit {
                parent: String::new(),
                name: format!("{:032x}", 8),
                kind: crate::unit::UnitKind::Directory,
                device: 16_777_219,
                inode: 4_242,
                marker: Some(crate::unit::UnitMarker::new(
                    crate::unit::MarkerFamily::Barge,
                    "b".repeat(64),
                )),
                root_id: None,
                capture_name: format!("{QUARANTINE_PREFIX}{intent_id}"),
                grant: crate::unit::UnitGrant::native_root(format!("{:032x}", 8)),
            },
            plan_digest: "c".repeat(64),
            phase: IntentPhase::Intent,
            generation: 1,
            recorded_ms: now,
            phase_ms: now,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
            identity: None,
            absence: None,
            caller_intent_id: None,
            caller_request_digest: None,
        }
    }
}
