//! The Oxide outputs adapter: the one deleter for Oxide's delegated build
//! outputs, acting only on a live Oxide prune intent (the joint protocol for
//! Barge output deletion, in Oxide's
//! `docs/knowledge/contracts/barge-adoption.md`, "Joint protocol for Barge
//! output deletion", design oxide#303; implemented under Tom's ruling
//! `63ad6fc4` v2 on disposable fixtures).
//!
//! What this module owns, and what nothing else in Barge may do:
//!
//! - **Construct** a [`crate::unit::MarkerFamily::OxideOutputs`] deletion unit.
//!   Its grant is the area-scoped field on the stored `oxide/outputs`
//!   declaration — never a `scratch_grants` row — and its unit is the exact
//!   root the live intent binds: volume UUID, inode, nonce and marker digest,
//!   never a device number alone.
//! - **Execute** the deletion, through the ordinary whole-unit path under the
//!   existing lock order (registry → exclusive volume → unit → intent write).
//!   The adapter never takes an Oxide admission or publication lock, never
//!   writes an Oxide journal, and does not re-read Oxide's pool ledger as a
//!   gate: Oxide's witnessed eligibility stays Oxide's, and Barge re-proves
//!   unit custody, the covered-class authority and the checkout's absence at
//!   effect.
//! - **Resume** a prior call under the same caller intent id, to the same
//!   journal record. A second call never starts a second unit, and the
//!   adapter's own per-intent replay is the only replay in Barge that may
//!   settle, resume or roll this family back.
//! - **Report** the outcome as one of a fixed set of facts, including the
//!   explicit known-no-effect `Locked` outcome of the reviewed same-call rule,
//!   and a bound [`lookup`] that lets Oxide settle or keep its own intent
//!   pending without inferring completion from missing bytes.
//!
//! Every refusal below any effect names why and changes nothing; every
//! unknown keeps the bytes (if still present) and both journals pending. No
//! outcome here is ever read from one journal as the other's conclusion.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::area::{self, OutputsGrant};
use crate::delete::{self, DeleteBounds, DeleteReport, DeletionAuthority, ReplayOutcome};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    self, AbsenceVolume, AccountFacts, AncestorExpected, DirOps, MountRoot, ProcessObserver,
    VolumeUuid,
};
use crate::pool_custody::{self, OutputsFence};
use crate::reference::{self, OxideIntent};
use crate::registry::{self, Durability};
use crate::state::{self, Deadline, IntentPhase, IntentRecord, LockSet, StatePaths};
use crate::unit::{DeletionUnit, MarkerFamily, UnitGrant, UnitKind, UnitMarker};

/// The namespace of the one area this adapter acts on.
pub const AREA_NAMESPACE: &str = "oxide";

/// The name of the one area this adapter acts on.
pub const AREA_NAME: &str = "outputs";

/// The area, as a message names it.
pub const AREA_REF: &str = "oxide/outputs";

/// The only Oxide journal phase a live intent may be in: the phase Oxide
/// publishes before it calls this adapter and holds until Barge is terminal
/// or a proven rollback. Any other phase — settled, or one of Oxide's own
/// local rename/delete phases — is outside the covered class and refused.
pub const LIVE_PHASE: &str = "intent";

/// How long one adapter call's Oxide observations may take: the same bound a
/// release plan's one reference observation takes.
pub const REFERENCE_WALL: Duration = reference::REFERENCE_WALL;

/// The covered marker version: an `oxide.outputs.root/2` document. A `/1`
/// marker predates the nonce and the checkout volume, and is never covered.
pub const COVERED_MARKER_VERSION: &str = reference::OXIDE_MARKER_V2;

/// The caller's claim about its own live prune intent.
///
/// This is a claim, never a permission: the adapter re-reads Oxide's registry
/// itself and refuses unless the stored document carries this intent, in this
/// phase, with this exact identity and preview digest. The field set mirrors
/// [`reference::OxideIntentIdentity`], so the comparison is one equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIntent {
    /// The caller's prune-intent id: Oxide's 128-bit lowercase-hex nonce.
    pub id: String,
    /// The journal phase the caller believes the intent is in; only
    /// [`LIVE_PHASE`] is accepted.
    pub phase: String,
    /// The preview digest that authorised the caller's move.
    pub preview_sha256: String,
    /// The intent's pinned root identity.
    pub identity: reference::OxideIntentIdentity,
}

/// The bound request: what the caller recorded when it read the stored
/// declaration's grant.
///
/// The digest material is canonical JSON over these fields, and the SHA-256
/// of those bytes is what both journals bind: the caller presents
/// `grant_generation`, the adapter recomputes the digest from the claim, and
/// the intent record stores it so a resume under a different request can
/// never continue this record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedRequest {
    /// The declaration generation whose grant the caller acted on. A stored
    /// generation older than this is a store that knows less than the caller:
    /// refused, never resolved in the caller's favour.
    pub grant_generation: u64,
}

impl DelegatedRequest {
    /// The request digest for one live intent: the caller's intent id, the
    /// intent's preview digest, the bound root identity and this generation,
    /// digested together.
    pub fn digest(&self, intent: &LiveIntent) -> Result<String> {
        let material = RequestMaterial {
            caller_intent_id: &intent.id,
            preview_sha256: &intent.preview_sha256,
            volume_uuid: intent.identity.volume_uuid.as_deref().unwrap_or(""),
            inode: intent.identity.ino,
            nonce: intent.identity.nonce.as_deref(),
            marker_digest: &intent.identity.marker_digest,
            root: &intent.identity.root,
            grant_generation: self.grant_generation,
        };
        let bytes = serde_json::to_vec(&material).map_err(|error| {
            Error::new(ErrorKind::Io, format!("cannot encode the request: {error}"))
        })?;
        Ok(sha256_hex(&bytes))
    }
}

/// The canonical digest material, in one fixed field order.
#[derive(Serialize)]
struct RequestMaterial<'a> {
    caller_intent_id: &'a str,
    preview_sha256: &'a str,
    volume_uuid: &'a str,
    inode: u64,
    nonce: Option<&'a str>,
    marker_digest: &'a str,
    root: &'a Path,
    grant_generation: u64,
}

/// The bounds one adapter call runs within: the state directory's own lock
/// wait, the deletion's bounds, and the Oxide observation wall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterBounds {
    /// How long one lock may wait for a live owner, as every other path's
    /// lock wait does.
    pub lock_wait: Duration,
    /// The deletion's own bounds: walk depth, custody caps, wall clock.
    pub delete: DeleteBounds,
    /// How long the one Oxide re-read may take.
    pub reference_wall: Duration,
}

impl AdapterBounds {
    /// The design's bounds: the state directory's 5 s lock wait, the default
    /// deletion bounds, and the 10 s reference wall.
    pub fn design() -> Self {
        Self {
            lock_wait: state::LOCK_WAIT,
            delete: DeleteBounds::default(),
            reference_wall: REFERENCE_WALL,
        }
    }
}

/// One adapter call: the process's facts, the live intent claim and the bound
/// request.
pub struct AdapterCall<'a> {
    /// The account's state directory.
    pub state_paths: &'a StatePaths,
    /// The platform's directory operations.
    pub dirs: &'a dyn DirOps,
    /// The volume facts, for absence proofs and mount lookups.
    pub facts: &'a dyn AbsenceVolume,
    /// The mount-root proof.
    pub mounts: &'a dyn MountRoot,
    /// The process facts, for custody.
    pub observer: &'a dyn ProcessObserver,
    /// The account, through which Oxide's own records are read in place.
    pub account: &'a dyn AccountFacts,
    /// The caller's live-intent claim.
    pub intent: &'a LiveIntent,
    /// The bound request.
    pub request: &'a DelegatedRequest,
    /// The call's bounds.
    pub bounds: AdapterBounds,
}

impl std::fmt::Debug for AdapterCall<'_> {
    /// The state directory and the claim; the process facts are trait objects
    /// with no readable identity, so they are named, not printed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterCall")
            .field("state", &self.state_paths.root().display().to_string())
            .field("caller_intent_id", &self.intent.id)
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

/// What one adapter call concluded.
///
/// `intent_id` is always **Barge's** id when one exists or was written: it
/// names the quarantine leaf `.barge-prune-<intent_id>`, and the caller
/// obtains the binding from this report. `caller_intent_id` is the caller's
/// own id, echoed so a report is readable without the call's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Which fact the call concluded.
    pub kind: OutcomeKind,
    /// The caller's own prune-intent id, as the call named it.
    pub caller_intent_id: String,
    /// The Barge intent id, when one exists or was written.
    pub intent_id: Option<String>,
    /// Why the call concluded as it did, when it is not a completion.
    pub reason: Option<String>,
}

/// The fact one adapter call concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// The unit is gone and the Barge intent is settled at `deleted`. This is
    /// a terminal completion, and the only fact from which Oxide may settle
    /// its own journal.
    Deleted,
    /// A Barge intent exists and is not settled. Its quarantine leaf is kept;
    /// the caller keeps its own intent pending.
    Pending,
    /// A pending Barge intent whose root was still in place was rolled back:
    /// the record is removed and the root is left exactly as it was. The
    /// caller may settle against this only after its own four-state lookup
    /// answers positively absent.
    RolledBack,
    /// Refused before any effect. No Barge intent exists for this caller id,
    /// and no byte moved. Empty, withdrawn, mismatched or stale authority, an
    /// uncovered legacy fence, a settled or mismatched intent, and a root
    /// that fails its re-proof all refuse here.
    Refused,
    /// This call's own `Locked` return, taken before any durable step of this
    /// call, with no Barge intent recorded for this caller id: the reviewed
    /// same-call rule's known no-effect fact. The volume lock is busy, this
    /// call wrote nothing, and nothing was renamed or deleted by it.
    KnownNoEffectLocked,
    /// A fact the call could not finish reading: an unreadable or incomplete
    /// journal, a lock it could not take, a volume that is gone, a call whose
    /// effect state is uncertain. Keep pending; never roll back, never infer.
    Unknown,
}

impl OutcomeKind {
    /// The kind's name, as a report spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Pending => "pending",
            Self::RolledBack => "rolled_back",
            Self::Refused => "refused",
            Self::KnownNoEffectLocked => "known_no_effect_locked",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the call provably wrote no journal record and moved no byte.
    pub const fn is_no_effect(self) -> bool {
        matches!(self, Self::Refused | Self::KnownNoEffectLocked)
    }
}

/// What Barge's journal answers for one caller intent id, to the bound
/// lookup Oxide settles against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BargeLookup {
    /// One matching record exists and is not settled; this is its id, phase
    /// and quarantine leaf's name.
    FoundPending {
        /// The Barge intent id, which names the quarantine leaf.
        intent_id: String,
        /// The phase the deletion reached.
        phase: String,
        /// The private `.barge-prune-<intent_id>` leaf inside the outputs
        /// folder, while the deletion is unfinished.
        quarantine_name: String,
    },
    /// One matching record exists and is settled at `deleted`.
    FoundTerminal {
        /// The Barge intent id.
        intent_id: String,
    },
    /// Every record was read completely under the volume's observation lock
    /// and none names this caller id, and the outputs folder holds no
    /// quarantine leaf that could be this operation's. This is a positive
    /// absence: the caller may treat "no journal" as a fact.
    PositivelyAbsent,
    /// The lookup could not finish: an unreadable or truncated journal, an
    /// unreadable record, an observation lock that was denied, a volume or
    /// folder that could not be read. Keep pending; never roll back and
    /// never settle against this.
    Unknown {
        /// Why, for a human.
        reason: String,
    },
}

/// Look one caller intent id up in Barge's journal, bounded.
///
/// The volume observation lock is taken shared first, so an in-flight write is
/// denied rather than raced; a denied lock is unknown, never absent. When the
/// caller holds the binding from an earlier report, pass it as
/// `barge_intent_id` and its leaf is the one that is checked; without it, any
/// quarantine leaf in the folder makes the lookup unknown, because a leaf
/// cannot be attributed to a record that was not read.
#[allow(clippy::too_many_arguments)]
pub fn lookup(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    caller_intent_id: &str,
    barge_intent_id: Option<&str>,
) -> BargeLookup {
    if let Err(reason) = validate_caller_id(caller_intent_id) {
        return BargeLookup::Unknown { reason };
    }
    let mut locks = LockSet::with_deadline(state_paths.clone(), Deadline::after(state::LOCK_WAIT));
    let _observation = match locks.observe_volume(volume_uuid) {
        Ok(lock) => lock,
        Err(error) => {
            return BargeLookup::Unknown {
                reason: format!(
                    "volume {}'s observation lock could not be taken, so the journal could not be read as a whole: {}",
                    volume_uuid,
                    error.message()
                ),
            }
        }
    };
    let intents = match state::read_intents(state_paths, crate::contract::INTENT_ROWS) {
        Ok(intents) => intents,
        Err(error) => {
            return BargeLookup::Unknown {
                reason: format!("the intent journal cannot be read as a whole: {error}"),
            }
        }
    };
    if intents.truncated {
        return BargeLookup::Unknown {
            reason: format!(
                "the intent journal holds more than {} records, so no absence can be positive",
                crate::contract::INTENT_ROWS
            ),
        };
    }
    let matches: Vec<&IntentRecord> = intents
        .records
        .iter()
        .filter(|record| record.caller_intent_id.as_deref() == Some(caller_intent_id))
        .collect();
    match matches.as_slice() {
        [record] => {
            let leaf = record.unit.capture_name.clone();
            return match record.phase {
                IntentPhase::Deleted => BargeLookup::FoundTerminal {
                    intent_id: record.intent_id.clone(),
                },
                phase => BargeLookup::FoundPending {
                    intent_id: record.intent_id.clone(),
                    phase: phase.as_str().to_owned(),
                    quarantine_name: leaf,
                },
            };
        }
        [] => {}
        [_, _, ..] => {
            return BargeLookup::Unknown {
                reason: format!(
                    "more than one intent record names caller id {caller_intent_id}, which no writer of this journal may produce"
                ),
            }
        }
    }
    // No record names this caller id. The absence is positive only once the
    // outputs folder shows no quarantine leaf that could be this operation's:
    // a leaf whose record cannot be read is a live deletion, not an absence.
    match quarantine_leaf(
        state_paths,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        barge_intent_id,
    ) {
        Ok(()) => BargeLookup::PositivelyAbsent,
        Err(reason) => BargeLookup::Unknown { reason },
    }
}

/// Whether the outputs folder names no quarantine leaf for this operation.
///
/// With the caller's binding, only that one leaf's absence is required. The
/// folder is resolved through the volume record and the stored declaration,
/// descriptor-relative, and the listing is bounded; a folder that cannot be
/// resolved or read is unknown, never leafless.
fn quarantine_leaf(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    barge_intent_id: Option<&str>,
) -> std::result::Result<(), String> {
    let record = registry::read(state_paths)
        .map_err(|error| format!("the registry cannot be read: {error}"))?
        .volume(volume_uuid)
        .cloned()
        .ok_or_else(|| {
            format!("volume {volume_uuid} is not registered, so its outputs folder cannot be read")
        })?;
    let document = area::show(state_paths, AREA_NAMESPACE, AREA_NAME)
        .map_err(|error| format!("the {AREA_REF} declaration cannot be read: {error}"))?
        .ok_or_else(|| {
            format!("the {AREA_REF} area is not declared, so its outputs folder cannot be read")
        })?;
    let folder = document
        .declaration()
        .folder_for(volume_uuid)
        .ok_or_else(|| {
            format!("the {AREA_REF} declaration resolves to no folder on volume {volume_uuid}")
        })?
        .to_owned();
    let resolved = area::open_folder_by(
        area::MountLookup::ByUuid(facts),
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &record.root_dir,
        &folder,
    )
    .map_err(|error| format!("the outputs folder does not verify: {error}"))?;
    let Some((_path, handle)) = resolved else {
        // The folder is positively not there. A leaf cannot be there either;
        // the caller still proves its own root's presence or absence, which
        // this answer does not decide for it.
        return Ok(());
    };
    let names = dirs
        .list_names(&handle, LEAF_ROWS)
        .map_err(|error| format!("the outputs folder cannot be listed: {error}"))?;
    if names.truncated {
        return Err(format!(
            "the outputs folder holds more than {LEAF_ROWS} names, so no leaf answer can be positive"
        ));
    }
    for name in &names.names {
        if !name.starts_with(state::QUARANTINE_PREFIX) {
            continue;
        }
        match barge_intent_id {
            Some(binding) if *name == format!("{}{binding}", state::QUARANTINE_PREFIX) => {
                return Err(format!(
                    "the quarantine leaf `{name}` for this operation is in the outputs folder, so the operation is not absent"
                ));
            }
            Some(_) => {}
            None => {
                return Err(format!(
                    "the outputs folder carries quarantine leaf `{name}`, which cannot be attributed without the operation's binding, so the lookup stays unknown"
                ));
            }
        }
    }
    Ok(())
}

/// How many names one leaf check lists before it refuses as unknown.
const LEAF_ROWS: usize = crate::contract::LISTING_NAME_ROWS;

/// Execute one adapter call.
///
/// The outcome is the call's whole answer: an effect-free refusal, a
/// completion, a pending intent, the same-call no-effect fact, or an unknown.
/// Only internal encoding failures surface as `Err`, and none of them follows
/// a durable step.
pub fn execute(call: &AdapterCall<'_>) -> Outcome {
    let intent = call.intent;
    let refused = |reason: String| Outcome {
        kind: OutcomeKind::Refused,
        caller_intent_id: intent.id.clone(),
        intent_id: None,
        reason: Some(reason),
    };
    let unknown = |reason: String| Outcome {
        kind: OutcomeKind::Unknown,
        caller_intent_id: intent.id.clone(),
        intent_id: None,
        reason: Some(reason),
    };
    // The claim's own shape. Every check here is before any lock, read of a
    // journal or effect, so each refusal is effect-free by construction.
    if let Err(reason) = validate_claim(intent) {
        return refused(reason);
    }
    let request_digest = match call.request.digest(intent) {
        Ok(digest) => digest,
        Err(error) => return unknown(error.message().to_owned()),
    };
    // The caller's volume UUID is part of the covered identity; unbound means
    // the identity predates the covered class.
    let Some(volume_uuid) = intent
        .identity
        .volume_uuid
        .as_deref()
        .map(VolumeUuid::parse)
        .transpose()
        .expect("the claim's volume UUID parses; validate_claim checked it")
    else {
        return refused(
            "the intent's identity records no volume UUID, so the root is not in the covered class"
                .to_owned(),
        );
    };

    // 1. Idempotency: this caller id names at most one record in this
    //    journal, and a record that exists decides the call before anything
    //    else runs.
    let intents = match state::read_intents(call.state_paths, crate::contract::INTENT_ROWS) {
        Ok(intents) => intents,
        Err(error) => {
            return unknown(format!("the intent journal cannot be read: {error}"));
        }
    };
    let mine: Vec<&IntentRecord> = intents
        .records
        .iter()
        .filter(|record| record.caller_intent_id.as_deref() == Some(intent.id.as_str()))
        .collect();
    let resumed = match mine.as_slice() {
        [] => None,
        [record] => {
            if record.phase == IntentPhase::Deleted {
                // Terminal: report the settled outcome. No lock, no re-read,
                // no second unit.
                return Outcome {
                    kind: OutcomeKind::Deleted,
                    caller_intent_id: intent.id.clone(),
                    intent_id: Some(record.intent_id.clone()),
                    reason: Some(
                        "the journal already records this operation's deletion as settled"
                            .to_owned(),
                    ),
                };
            }
            if record.caller_request_digest.as_deref() != Some(request_digest.as_str()) {
                return refused(format!(
                    "a pending intent already names caller id {} for a different request, so this request is refused",
                    intent.id
                ));
            }
            Some(record.intent_id.clone())
        }
        [_, _, ..] => {
            return unknown(format!(
                "more than one intent record names caller id {}, which no writer of this journal may produce",
                intent.id
            ));
        }
    };

    // 2. The stored authority: the declaration and its area-scoped grant, read
    //    from the state directory, never from the caller's objects.
    let document = match read_authority(call, call.request.grant_generation) {
        Ok(document) => document,
        Err(answer) => return answer,
    };

    // 3. The volume's own record: registered and expendable. Everything the
    //    delete path re-reads is re-read there under the locks; this read
    //    picks the folder the unit is named in.
    let record = match registry::read(call.state_paths) {
        Ok(registry) => registry,
        Err(error) => return unknown(format!("the registry cannot be read: {error}")),
    };
    let Some(record) = record.volume(&volume_uuid).cloned() else {
        return refused(format!(
            "volume {volume_uuid} is not registered, so nothing on it can be acted on"
        ));
    };
    if record.durability != Durability::Expendable {
        return refused(format!(
            "volume {volume_uuid} is declared `{}`, so none of its data is expendable and the covered class does not reach it",
            match record.durability {
                Durability::Expendable => "expendable",
                Durability::Kept => "kept",
                Durability::Undeclared => "undeclared",
            }
        ));
    }

    // 4. The live intent, re-read from Oxide's own registry. The caller's
    //    object is the claim; this stored document is the permission.
    match lookup_oxide_intent(call.account, call.bounds.reference_wall, intent) {
        OxideLookup::Found(stored) => {
            if let Err(reason) = compare_stored_intent(intent, &stored) {
                return refused(reason);
            }
        }
        OxideLookup::Absent => {
            return refused(
                "the caller's intent is not in Oxide's completely read registry, so no live intent authorises this deletion"
                    .to_owned(),
            );
        }
        OxideLookup::NotCovered { reason } => return refused(reason),
        OxideLookup::Unknown { reason } => return unknown(reason),
    }

    // 5. The unit: one directory, directly inside the declared folder, bound
    //    by inode, nonce, marker bytes and volume UUID. The request digest is
    //    the plan digest the journal records.
    let folder = match document.declaration().folder_for(&volume_uuid) {
        Some(folder) => folder.to_owned(),
        None => {
            return refused(format!(
                "the {AREA_REF} declaration resolves to no folder on volume {volume_uuid}"
            ));
        }
    };
    let Some(name) = intent
        .identity
        .root
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return refused(format!(
            "the intent's root {} has no usable entry name inside its outputs folder",
            intent.identity.root.display()
        ));
    };
    let unit = DeletionUnit {
        volume_uuid: volume_uuid.clone(),
        root_dir: record.root_dir.clone(),
        folder,
        parent: String::new(),
        name: name.to_owned(),
        root_id: None,
        kind: UnitKind::Directory,
        device: crate::os::DeviceId::new(intent.identity.dev),
        inode: intent.identity.ino,
        marker: Some(UnitMarker::new(
            MarkerFamily::OxideOutputs,
            intent.identity.marker_digest.clone(),
        )),
        grant: UnitGrant::delegation(MarkerFamily::OxideOutputs),
        plan_digest: request_digest.clone(),
    };
    if let Err(error) = unit.validate() {
        return refused(format!(
            "the covered root's unit is not a valid deletion unit: {error}"
        ));
    }

    // 6. The exclusive volume lock. A denied lock is decided by the same-call
    //    rule, and the journal is re-read after the denial so the answer is
    //    about this call and not about a scan that has already aged.
    let mut locks = LockSet::with_deadline(
        call.state_paths.clone(),
        Deadline::after(call.bounds.lock_wait),
    );
    let volume_lock = match locks.lock_volume(&volume_uuid) {
        Ok(lock) => lock,
        Err(error) if error.kind() == ErrorKind::Locked => {
            return locked_outcome(call, &intent.id, resumed.as_deref(), &error);
        }
        Err(error) => {
            return unknown(format!(
                "volume {}'s lock could not be taken: {}",
                volume_uuid,
                error.message()
            ));
        }
    };

    // 7. A resumed call replays its own record: the same journal record the
    //    first call wrote, through the same preflight, absence and custody
    //    rules as every other replay, and nothing else on the volume.
    if let Some(intent_id) = resumed {
        return resume(call, intent_id, &volume_lock);
    }

    // 8. The re-proof the effect runs under the locks, before its intent:
    //    the stored grant again, the exact marker again — version, digest,
    //    nonce, volume UUID, checkout — and the checkout's absence.
    let refusal: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let authorize = |record: &registry::VolumeRecord| -> Result<()> {
        match reprove(call, &volume_uuid, record, &unit, intent) {
            Ok(()) => Ok(()),
            Err(reason) => {
                *refusal.borrow_mut() = Some(reason.clone());
                Err(Error::new(
                    ErrorKind::RefusedBoundary,
                    format!("the covered root was kept before its intent was written: {reason}"),
                )
                .with_detail("volume_uuid", record.volume_uuid.as_str().into()))
            }
        }
    };
    let declaration = document.declaration();
    let outcome = delete::delete_unit_under_volume_lock(
        call.state_paths,
        call.dirs,
        call.facts,
        call.mounts,
        call.observer,
        area::MountLookup::ByUuid(call.facts),
        &declaration,
        &unit,
        call.bounds.delete,
        &volume_lock,
        DeletionAuthority::OxideOutputs,
        Some(&authorize),
        &state::IntentEvidence {
            caller: Some(state::CallerCorrelation {
                caller_intent_id: intent.id.clone(),
                request_digest,
            }),
            ..state::IntentEvidence::none()
        },
    );
    match outcome {
        Ok(report) => report_outcome(&report, &intent.id),
        Err(error) => {
            // A refusal the re-proof made happened before any intent, so it is
            // effect-free and names its reason. An `Io` error can have
            // happened at the intent publish itself, where atomicity is the
            // filesystem's, so its state is uncertain and stays unknown. A
            // lock the inner path could not take may mean a prior attempt is
            // in flight, which is also unknown rather than no-effect.
            if let Some(reason) = refusal.borrow().clone() {
                return refused(reason);
            }
            match error.kind() {
                ErrorKind::Io | ErrorKind::Locked => {
                    unknown(format!(
                        "the deletion ended before its journal state could be read back, so its state is uncertain: {}",
                        error.message()
                    ))
                }
                _ => refused(format!(
                    "the deletion refused before any effect: {}",
                    error.message()
                )),
            }
        }
    }
}

/// The outcome of a `Locked` volume lock, decided by the same-call rule.
///
/// The journal is read again after the denial: an intent for this caller id
/// that a prior call wrote makes this call a resumed one, whose `Locked` is
/// unknown — a prior attempt may already hold a quarantine — and never a
/// no-effect fact. With none, this call wrote nothing, took no root lock and
/// renamed nothing, which is exactly the reviewed first-call fact.
fn locked_outcome(
    call: &AdapterCall<'_>,
    caller_id: &str,
    resumed: Option<&str>,
    error: &Error,
) -> Outcome {
    let base = |kind: OutcomeKind, reason: String| Outcome {
        kind,
        caller_intent_id: caller_id.to_owned(),
        intent_id: resumed.map(str::to_owned),
        reason: Some(reason),
    };
    if resumed.is_some() {
        return base(
            OutcomeKind::Pending,
            format!(
                "the volume lock is busy and a prior intent for this caller id already exists, so the operation stays pending: {}",
                error.message()
            ),
        );
    }
    let still_fresh = match state::read_intents(call.state_paths, crate::contract::INTENT_ROWS) {
        Ok(intents) => !intents
            .records
            .iter()
            .any(|record| record.caller_intent_id.as_deref() == Some(caller_id)),
        Err(_) => false,
    };
    if !still_fresh {
        return base(
            OutcomeKind::Unknown,
            format!(
                "the volume lock is busy and the journal could not be re-read to prove this call wrote nothing, so the state is uncertain: {}",
                error.message()
            ),
        );
    }
    base(
        OutcomeKind::KnownNoEffectLocked,
        format!(
            "the volume lock is busy and this call wrote no journal record, renamed nothing and deleted nothing, so its own `Locked` return is the no-effect fact: {}",
            error.message()
        ),
    )
}

/// Replay one pending record to its next fact, under the held volume lock.
fn resume(
    call: &AdapterCall<'_>,
    intent_id: String,
    // Held for the whole replay: the per-intent replay verifies it is this
    // volume's exclusive lock, and dropping it early would let a concurrent
    // writer in between the checks and the journal.
    _volume_lock: &crate::state::StateLock,
) -> Outcome {
    let intent = call.intent;
    let outcome_for = |kind: OutcomeKind, reason: Option<String>| Outcome {
        kind,
        caller_intent_id: intent.id.clone(),
        intent_id: Some(intent_id.clone()),
        reason,
    };
    // The live intent must still be live: a settled or advanced Oxide intent
    // is refused, and the exclusion the caller dropped stays dropped until the
    // journal record is settled by its own adapter.
    match lookup_oxide_intent(call.account, call.bounds.reference_wall, intent) {
        OxideLookup::Found(stored) => {
            if let Err(reason) = compare_stored_intent(intent, &stored) {
                return outcome_for(OutcomeKind::Refused, Some(reason));
            }
        }
        OxideLookup::Absent => {
            return outcome_for(
                OutcomeKind::Refused,
                Some(
                    "the caller's intent is no longer in Oxide's completely read registry, so this resume is refused; the pending intent stays"
                        .to_owned(),
                ),
            );
        }
        OxideLookup::NotCovered { reason } => {
            return outcome_for(OutcomeKind::Refused, Some(reason))
        }
        OxideLookup::Unknown { reason } => return outcome_for(OutcomeKind::Unknown, Some(reason)),
    }
    // The record decides the volume, exactly as its own writer recorded it.
    let journal = match state::read_intent(call.state_paths, &intent_id) {
        Ok(record) => record,
        Err(error) => {
            return outcome_for(
                OutcomeKind::Unknown,
                Some(format!(
                    "the pending intent {intent_id} cannot be read back: {error}"
                )),
            );
        }
    };
    let volume_uuid = journal.volume_uuid.clone();
    let record = match registry::read(call.state_paths) {
        Ok(registry) => registry,
        Err(error) => {
            return outcome_for(
                OutcomeKind::Unknown,
                Some(format!("the registry cannot be read: {error}")),
            );
        }
    };
    let Some(record) = record.volume(&volume_uuid).cloned() else {
        return outcome_for(
            OutcomeKind::Pending,
            Some(format!(
                "volume {volume_uuid} is no longer registered, so the pending intent stays"
            )),
        );
    };
    match delete::replay_intent_under_volume_lock(
        call.state_paths,
        call.dirs,
        call.facts,
        call.observer,
        &record,
        &journal,
        call.bounds.delete,
    ) {
        Ok(ReplayOutcome::Settled)
        | Ok(ReplayOutcome::SettledWithRemoval { .. })
        | Ok(ReplayOutcome::Resumed { .. }) => outcome_for(OutcomeKind::Deleted, None),
        Ok(ReplayOutcome::RolledBack) => outcome_for(OutcomeKind::RolledBack, None),
        Ok(ReplayOutcome::Partial { reason, .. }) | Ok(ReplayOutcome::Pending(reason)) => {
            outcome_for(OutcomeKind::Pending, Some(reason))
        }
        Err(error) => match error.kind() {
            ErrorKind::Io | ErrorKind::Locked => outcome_for(
                OutcomeKind::Unknown,
                Some(format!(
                    "the replay's journal state is uncertain: {}",
                    error.message()
                )),
            ),
            _ => outcome_for(
                OutcomeKind::Pending,
                Some(format!(
                    "the replay kept the intent pending: {}",
                    error.message()
                )),
            ),
        },
    }
}

/// Map one delete report to its outcome.
fn report_outcome(report: &DeleteReport, caller_id: &str) -> Outcome {
    let intent_id = Some(report.intent_id.clone());
    let reason = report.reason.clone();
    match report.outcome {
        delete::DeleteOutcome::Deleted => Outcome {
            kind: OutcomeKind::Deleted,
            caller_intent_id: caller_id.to_owned(),
            intent_id,
            reason: None,
        },
        delete::DeleteOutcome::Pending => Outcome {
            kind: OutcomeKind::Pending,
            caller_intent_id: caller_id.to_owned(),
            intent_id,
            reason,
        },
        delete::DeleteOutcome::RolledBack => Outcome {
            kind: OutcomeKind::RolledBack,
            caller_intent_id: caller_id.to_owned(),
            intent_id,
            reason,
        },
    }
}

/// Read the stored declaration and its grant, against the caller's recorded
/// generation.
///
/// The error is the whole refusal or unknown outcome: an undeclared area, an
/// unreadable record, a withdrawn grant, a grant whose family is not the
/// covered one, and a generation older than the caller's recorded one all
/// answer without any effect.
fn read_authority(
    call: &AdapterCall<'_>,
    recorded_generation: u64,
) -> std::result::Result<area::AreaDocument, Outcome> {
    let id = call.intent.id.clone();
    let refused = |reason: String| Outcome {
        kind: OutcomeKind::Refused,
        caller_intent_id: id.clone(),
        intent_id: None,
        reason: Some(reason),
    };
    let document = match area::show(call.state_paths, AREA_NAMESPACE, AREA_NAME) {
        Ok(Some(document)) => document,
        Ok(None) => {
            return Err(refused(format!(
                        "the {AREA_REF} area is not declared, so nothing on any volume is covered and no deletion is authorised"
                    )));
        }
        Err(error) => {
            return Err(Outcome {
                kind: OutcomeKind::Unknown,
                caller_intent_id: id,
                intent_id: None,
                reason: Some(format!(
                    "the {AREA_REF} declaration cannot be read, so the authority is unknown: {error}"
                )),
            });
        }
    };
    let Some(grant) = &document.outputs_grant else {
        return Err(refused(format!(
                    "the {AREA_REF} declaration carries no outputs grant at generation {}, so the authority is withdrawn and nothing is authorised",
                    document.generation
                )));
    };
    if grant.family != OutputsGrant::FAMILY {
        return Err(refused(format!(
                    "the {AREA_REF} declaration's grant covers `{}`, not `{}`, so the authority does not match this family",
                    grant.family,
                    OutputsGrant::FAMILY
                )));
    }
    if document.generation < recorded_generation {
        return Err(refused(format!(
                    "the {AREA_REF} declaration is at generation {}, older than the caller's recorded grant generation {recorded_generation}, so the record is not the authority the request names",
                    document.generation
                )));
    }
    Ok(document)
}

/// Re-prove the covered root under the held locks, before the intent: the
/// stored grant, the exact marker — version, bytes, nonce, volume UUID,
/// checkout — and the checkout's absence. The `String` is the keep reason.
fn reprove(
    call: &AdapterCall<'_>,
    volume_uuid: &VolumeUuid,
    record: &registry::VolumeRecord,
    unit: &DeletionUnit,
    intent: &LiveIntent,
) -> std::result::Result<(), String> {
    // The grant is read again from the state directory under the volume lock,
    // immediately before the intent write: a re-declare that withdrew it
    // between the first read and here keeps the root.
    let document = area::show(call.state_paths, AREA_NAMESPACE, AREA_NAME)
        .map_err(|error| format!("the {AREA_REF} declaration cannot be re-read: {error}"))?
        .ok_or_else(|| format!("the {AREA_REF} area is no longer declared"))?;
    match &document.outputs_grant {
        Some(grant) if grant.family == OutputsGrant::FAMILY => {}
        Some(grant) => {
            return Err(format!(
                "the grant now covers `{}`, not `{}`",
                grant.family,
                OutputsGrant::FAMILY
            ));
        }
        None => {
            return Err(format!(
                "the {AREA_REF} declaration carries no outputs grant at generation {}, so the authority was withdrawn",
                document.generation
            ));
        }
    }
    if document.generation < call.request.grant_generation {
        return Err(format!(
            "the declaration is at generation {}, older than the caller's recorded grant generation {}",
            document.generation, call.request.grant_generation
        ));
    }

    // The exact root: the folder is resolved from this record, the unit's
    // entry is opened descriptor-relative, and the marker is read through the
    // descriptor. An `oxide.outputs.root/1` marker is refused here by the
    // reader itself; a digest that differs means these are not the bytes the
    // intent bound.
    let (area_path, area_handle) = area::open_folder_by(
        area::MountLookup::ByUuid(call.facts),
        call.dirs,
        call.facts,
        call.mounts,
        call.observer,
        record,
        &unit.root_dir,
        &unit.folder,
    )
    .map_err(|error| format!("the outputs folder does not verify: {error}"))?
    .ok_or_else(|| "the outputs folder is not there".to_owned())?;
    let Some(parent) = unit
        .open_parent(call.dirs, &area_handle)
        .map_err(|error| format!("the unit's parent cannot be opened: {error}"))?
    else {
        return Err("the unit's parent is not there".to_owned());
    };
    let entry = unit.path_in(&area_path);
    let handle = call
        .dirs
        .open_child_dir(&parent, &unit.name)
        .map_err(|error| format!("the root at {} cannot be opened: {error}", entry.display()))?;
    let marker = reference::read_oxide_marker_full(call.dirs, &handle)
        .map_err(|reason| {
            format!("the root's Oxide marker cannot be read as the covered version: {reason}")
        })?
        .ok_or_else(|| format!("the root at {} carries no Oxide marker", entry.display()))?;
    if marker.digest != intent.identity.marker_digest {
        return Err(format!(
            "the root's marker bytes are digest {}, not the {} the intent bound",
            marker.digest, intent.identity.marker_digest
        ));
    }
    if marker.nonce.as_deref() != intent.identity.nonce.as_deref() {
        return Err(format!(
            "the root's marker nonce is {:?}, not the one the intent bound",
            marker.nonce
        ));
    }
    let marker_volume = marker.volume_uuid.as_ref().map(VolumeUuid::as_str);
    if marker_volume != Some(volume_uuid.as_str()) {
        return Err(format!(
            "the root's marker names volume {:?}, not this volume {}",
            marker_volume, volume_uuid
        ));
    }
    if marker.checkout_id != intent.identity.checkout_id {
        return Err(format!(
            "the root's marker names checkout id {}, not the one the intent bound",
            marker.checkout_id
        ));
    }
    if marker.checkout != intent.identity.checkout {
        return Err(format!(
            "the root's marker names checkout {}, not the one the intent bound",
            marker.checkout.display()
        ));
    }
    let identity = handle.identity();
    if identity.inode != unit.inode {
        return Err(format!(
            "the root's inode is now {}, not the {} the intent bound",
            identity.inode, unit.inode
        ));
    }

    // The checkout's absence, the fact Oxide required for coverage, proved
    // again now through Barge's own absence primitive. The intent's identity
    // names no checkout volume, so a checkout under a mount parent cannot be
    // proved against a registered volume here and refuses; a checkout under
    // the boot volume proves normally.
    let proof = prove_checkout_absent(call.dirs, call.facts, &intent.identity.checkout).map_err(
        |error| {
            format!(
                "the checkout's absence cannot be proved: {}",
                error.message()
            )
        },
    )?;
    if proof != "absent" {
        return Err(format!(
            "the checkout {} is not positively absent (verdict `{proof}`), so the covered premise does not hold",
            intent.identity.checkout.display()
        ));
    }
    let _ = area_path;
    Ok(())
}

/// Prove one checkout path absent.
///
/// The same rule a release plan's absence proof takes: a path under the
/// platform's mount parent needs the volume its record names, and the intent
/// identity carries none, so such a checkout refuses; a path without a mount
/// parent is proved against the boot volume. The verdict string is the
/// proof's own.
fn prove_checkout_absent(
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    checkout: &Path,
) -> Result<String> {
    match facts.volume_prefix(checkout) {
        Some(_) => Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the checkout {} is under a mount parent its intent does not name, so its absence cannot be proved against a registered volume",
                checkout.display()
            ),
        )),
        None => {
            let proof = os::prove_absence(dirs, facts, checkout, AncestorExpected::BootData)?;
            Ok(proof.verdict().to_owned())
        }
    }
}

/// What Oxide's own records answer for one caller intent id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OxideLookup {
    /// The stored registry carries the intent.
    Found(OxideIntent),
    /// The registry was read completely and the id is not in it.
    Absent,
    /// The pools are readable and not on the covered class: a legacy or
    /// absent fence. A refusal, never an unknown.
    NotCovered { reason: String },
    /// Something made the read inconclusive.
    Unknown { reason: String },
}

/// Read the caller's intent from Oxide's own records, bounded, and decide the
/// covered class's fence on the way.
///
/// `Ok(None)` from the control-directory search is **unknown**, never absent:
/// the caller claims a live intent, so a world where no record can be read
/// cannot positively contradict it. Positive absence requires a control
/// directory, a fence decision, and a registry that parsed completely with
/// this id not in it.
fn lookup_oxide_intent(
    account: &dyn AccountFacts,
    wall: Duration,
    intent: &LiveIntent,
) -> OxideLookup {
    let unknown = |reason: String| OxideLookup::Unknown { reason };
    let deadline = Deadline::after(wall);
    let control = match reference::control_directory(account, deadline) {
        Ok(Some(control)) => control,
        Ok(None) => {
            return unknown(
                "no Oxide control directory exists, so the caller's live intent cannot be read anywhere"
                    .to_owned(),
            );
        }
        Err(error) => {
            return unknown(format!(
                "Oxide's control directory cannot be found: {}",
                error.message()
            ))
        }
    };
    let pools = match reference::pools_holding(&control) {
        Ok(pools) => pools,
        Err(reason) => return unknown(reason),
    };
    match pool_custody::owned_outputs_fence(&pools, deadline) {
        OutputsFence::Covered => {}
        OutputsFence::NotCovered { reason } => return OxideLookup::NotCovered { reason },
        OutputsFence::Unknown { reason } => return unknown(reason),
    }
    let registry = reference::read_registry(
        &control
            .join(reference::OXIDE_OUTPUTS_DIR)
            .join(reference::OXIDE_REGISTRY_FILE),
        deadline,
    );
    match registry {
        reference::OxideRegistry::Read {
            rows: _,
            intents,
            complete,
        } => {
            if !complete {
                return unknown(
                    "Oxide's outputs registry reports itself incomplete, so no intent in it can be decided"
                        .to_owned(),
                );
            }
            match intents.iter().find(|stored| stored.id == intent.id) {
                Some(stored) => OxideLookup::Found(stored.clone()),
                None => OxideLookup::Absent,
            }
        }
        reference::OxideRegistry::Absent => unknown(
            "Oxide's outputs registry is not there, so the caller's live intent cannot be read"
                .to_owned(),
        ),
        reference::OxideRegistry::Unknown { reason } => {
            unknown(format!("Oxide's outputs registry cannot be read: {reason}"))
        }
    }
}

/// Compare the stored intent with the caller's claim, field for field.
///
/// A settled or advanced phase refuses, and so does any identity or preview
/// difference: this adapter acts on one exact intent, not on one that looks
/// like it.
fn compare_stored_intent(
    intent: &LiveIntent,
    stored: &OxideIntent,
) -> std::result::Result<(), String> {
    if stored.phase != intent.phase {
        return Err(format!(
            "the stored intent is in phase `{}`, not the live phase `{}` the caller named, so it is not the intent this adapter acts on",
            stored.phase, intent.phase
        ));
    }
    if stored.preview_sha256 != intent.preview_sha256 {
        return Err("the stored intent's preview digest differs from the caller's".to_owned());
    }
    let identity = &stored.identity;
    let claim = &intent.identity;
    let same = identity.checkout_id == claim.checkout_id
        && identity.checkout == claim.checkout
        && identity.receipt == claim.receipt
        && identity.member == claim.member
        && identity.root == claim.root
        && identity.root_kind == claim.root_kind
        && identity.volume_uuid == claim.volume_uuid
        && identity.nonce == claim.nonce
        && identity.generation == claim.generation
        && identity.dev == claim.dev
        && identity.ino == claim.ino
        && identity.marker_digest == claim.marker_digest;
    if same {
        Ok(())
    } else {
        Err(format!(
            "the stored intent's identity differs from the caller's claim, so this adapter acts on neither: the stored intent names root {} at inode {}",
            identity.root.display(),
            identity.ino
        ))
    }
}

/// Whether one claim is shaped like the covered class's live intent.
///
/// The id is one of Oxide's 128-bit lowercase-hex nonces, the phase is the
/// live phase, both digests are SHA-256 hex, the root identity binds an inode,
/// a nonce and a marker digest, and the volume UUID is present and parses.
/// Anything else is refused before any read.
fn validate_claim(intent: &LiveIntent) -> std::result::Result<(), String> {
    validate_caller_id(&intent.id)?;
    if intent.phase != LIVE_PHASE {
        return Err(format!(
            "the caller's intent is in phase `{}`, not the live phase `{LIVE_PHASE}`; a settled or locally-advancing intent is never this adapter's",
            intent.phase
        ));
    }
    if !crate::unit::is_sha256(&intent.preview_sha256) {
        return Err("the caller's preview digest is not a SHA-256 hex string".to_owned());
    }
    let identity = &intent.identity;
    if identity.ino == 0 {
        return Err("the intent's identity binds no inode".to_owned());
    }
    if !crate::unit::is_sha256(&identity.marker_digest) {
        return Err("the intent's marker digest is not a SHA-256 hex string".to_owned());
    }
    if identity.nonce.is_none() {
        return Err(
            "the intent's identity binds no root nonce, which the covered class requires"
                .to_owned(),
        );
    }
    match &identity.volume_uuid {
        Some(raw) => {
            VolumeUuid::parse(raw).map_err(|error| {
                format!(
                    "the intent's volume UUID is not a UUID: {}",
                    error.message()
                )
            })?;
        }
        None => {
            return Err(
                "the intent's identity binds no volume UUID, which the covered class requires"
                    .to_owned(),
            );
        }
    }
    if identity.generation == 0 {
        return Err(
            "the intent's identity binds generation zero, which no root Oxide writes".to_owned(),
        );
    }
    if !identity.root.is_absolute() {
        return Err(format!(
            "the intent's root {} is not an absolute path",
            identity.root.display()
        ));
    }
    Ok(())
}

/// Whether one string is an Oxide prune-intent id: 128 bits of lowercase hex.
fn validate_caller_id(id: &str) -> std::result::Result<(), String> {
    if id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Ok(());
    }
    Err(format!(
        "`{id}` is not an Oxide prune-intent id (32 lowercase hex digits)"
    ))
}

#[cfg(test)]
#[path = "oxide_outputs/tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
