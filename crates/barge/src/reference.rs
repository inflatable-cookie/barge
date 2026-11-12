//! Reference readers: is any record still naming this root?
//!
//! The release gate asks one question the identity and absence primitives
//! cannot: does every reference that names this owned root say it is no longer
//! wanted? This module answers it from two places, and from nowhere else:
//!
//! - **Barge's own authoritative state.** A placement record, an area
//!   declaration and a hold record. A placement whose recorded path is this root
//!   is current, a declaration that does not permit releasing roots past its age
//!   is current, and a hold that is live or undecidable is current. A stale hold
//!   is not: custody has already classified it as positively gone.
//! - **Oxide's own records, read strictly in place** (Q-004): the outputs
//!   registry `oxide.outputs.registry/1`–`/4`, each in exactly the field set
//!   its version defines (`/3` requires its `deletion_routes`; `/4` requires
//!   its retained `settled` set and carries no routes), the checkout receipts
//!   `oxide.outputs.transition/1..3`, and one root marker
//!   `oxide.outputs.root/2` (`/1` is read as unknown). Nothing is written,
//!   nothing is adopted and no Oxide fact becomes authoritative. A row that is
//!   selected, pending, reserved, pruning or cancelling is current; Oxide's own
//!   [`Row::live`] predicate includes `pending`, and this reader keeps that
//!   stricter reading so a row Oxide has not settled never releases a root.
//! - **Oxide's pool writer custody** (R4, [`crate::pool_custody`]): each pool's
//!   control state and ledger, read once. A checkout with a live writer is
//!   current; a checkout whose writers cannot be read as closed keeps its set
//!   incomplete. Closed adds no row.
//!
//! Two rules run through every reader:
//!
//! - **Unknown is never "no references".** A record that is unreadable,
//!   oversized, schema-divergent or an unknown version makes the set it belongs
//!   to incomplete, and an incomplete set keeps the whole batch it names.
//! - **The absence proof belongs to the reference, not the root.** A reference
//!   whose own rule is "the consumer path is gone" (an Oxide receipt or registry
//!   row that no longer selects this root) contributes the path whose
//!   `barge.absence/1` proof the gate must establish.
//!
//! Every bound here is the stage 4 design's: Oxide's own registry cap, one
//! 64 KiB read per root path, 10 s, and at most [`RECEIPT_ROWS`] receipts.

use std::path::{Path, PathBuf};
// The metadata facts the cfg(test) tape rows record.
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::area::{self, AreaDocument, HoldReading, PrunePolicy};
use crate::cache::{self, PlacementForm, PlacementSet};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{AccountFacts, DirHandle, DirOps, VolumeUuid};
use crate::pool_custody::{CoordinatedAdoption, Custody, PoolCustody, REMOVABLE_POOL_DIR};
use crate::state::{self, Bounds, Deadline, ReadOutcome, StatePaths};

/// A reference from one of Barge's own records.
pub const BARGE_PLACEMENT: &str = "barge_placement";
/// A reference from an area declaration.
pub const BARGE_AREA: &str = "barge_area";
/// A reference from a hold record.
pub const BARGE_HOLD: &str = "barge_hold";
/// A reference read from an Oxide checkout receipt.
pub const OXIDE_RECEIPT: &str = "oxide_receipt";
/// A reference read from Oxide's outputs registry.
pub const OXIDE_REGISTRY: &str = "oxide_registry";
/// Ownership recorded by an Oxide root marker.
pub const OXIDE_MARKER: &str = "oxide_marker";
/// A checkout's writer custody, read from Oxide's pool state and ledger. Held
/// names the root as wanted; unknown keeps the set incomplete.
pub const OXIDE_WRITER_CUSTODY: &str = "oxide_writer_custody";

/// Oxide's explicit deletion-route registry.
pub const OXIDE_REGISTRY_V3: &str = "oxide.outputs.registry/3";
/// Oxide's coordinated single-authority registry. Its `settled` array is the
/// retained terminal record of delegated deletions, read and validated through
/// [`crate::oxide_settlement`], and its `deletion_routes` member is gone.
pub const OXIDE_REGISTRY_V4: &str = "oxide.outputs.registry/4";
/// The historical outputs registry without deletion routes.
pub const OXIDE_REGISTRY_V2: &str = "oxide.outputs.registry/2";
/// Oxide's predecessor outputs registry, read as its own field set.
pub const OXIDE_REGISTRY_V1: &str = "oxide.outputs.registry/1";
/// Oxide's current root marker.
pub const OXIDE_MARKER_V2: &str = "oxide.outputs.root/2";
/// Oxide's predecessor root marker. Its field set predates the nonce and the
/// checkout volume, so this reader reports it as unknown rather than guessing.
pub const OXIDE_MARKER_V1: &str = "oxide.outputs.root/1";
/// One unfinished prune journal entry's format.
pub const OXIDE_INTENT_V1: &str = "oxide.outputs.prune-intent/1";
/// Oxide's own text bound for a member or a recipe name.
pub const MAX_TEXT_BYTES: usize = 1024;

/// Whether a value is one of Oxide's 128-bit lowercase hex ids.
pub(crate) fn is_hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether a value is one of Oxide's 256-bit lowercase hex digests.
pub(crate) fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether a path is a *usable* absolute path: absolute, with no `.` or `..`
/// component.
///
/// It is Oxide's own `safe_absolute` for a receipt's root paths. A path with a parent
/// component is never the component chain of a root Barge walks, so accepting one
/// would let a snapshot look complete while a root it names looks unreferenced: the
/// document is divergent instead.
pub(crate) fn is_safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
}

/// Whether a value is usable identifying text: non-empty, bounded and free of NUL.
fn is_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TEXT_BYTES && !value.contains('\0')
}

/// The two root kinds Oxide's own document accepts. A value outside this set is a
/// document this build does not understand, never a root of an unknown kind.
pub const OXIDE_ROOT_KINDS: [&str; 2] = ["external", "local"];

/// Whether a root-kind value is one Oxide writes.
fn check_root_kind(what: &str, kind: &str) -> std::result::Result<(), String> {
    if OXIDE_ROOT_KINDS.contains(&kind) {
        return Ok(());
    }
    Err(format!(
        "{what} states root kind `{kind}`, which is not one Oxide writes"
    ))
}
/// Oxide's whole-target receipt.
pub const OXIDE_RECEIPT_V1: &str = "oxide.outputs.transition/1";
/// Oxide's committed-receipt form.
pub const OXIDE_RECEIPT_V2: &str = "oxide.outputs.transition/2";
/// Oxide's profile-receipt form.
pub const OXIDE_RECEIPT_V3: &str = "oxide.outputs.transition/3";

/// The marker file inside one Oxide output root.
pub const OXIDE_MARKER_FILE: &str = ".oxide-output.json";
/// The outputs folder inside Oxide's control directory.
pub const OXIDE_OUTPUTS_DIR: &str = "outputs";
/// The registry file inside that folder.
pub const OXIDE_REGISTRY_FILE: &str = "registry.json";

/// Oxide's own registry byte cap.
pub const REGISTRY_CAP_BYTES: usize = 4 * 1024 * 1024;
/// Oxide's own receipt byte cap.
pub const RECEIPT_CAP_BYTES: usize = 64 * 1024;
/// Oxide's own marker byte cap.
pub const MARKER_CAP_BYTES: usize = 64 * 1024;
/// Oxide's own registry row cap.
pub const REGISTRY_ROWS: usize = 4096;
/// How many receipts one reference snapshot reads.
pub const RECEIPT_ROWS: usize = 4096;
/// The whole time one reference snapshot may take.
pub const REFERENCE_WALL: Duration = Duration::from_secs(10);

/// One reference naming a root, as a plan row reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRow {
    /// Where the reference was read: one of this module's `source` names.
    pub source: String,
    /// Whether the reference still names the root as wanted.
    pub current: bool,
    /// The consumer path whose `barge.absence/1` proof this reference's own rule
    /// requires, when it requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// What was read, for a human. It never carries a decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ReferenceRow {
    fn new(source: &'static str, current: bool) -> Self {
        Self {
            source: source.to_owned(),
            current,
            path: None,
            detail: None,
        }
    }

    fn with_path(mut self, path: &Path) -> Self {
        self.path = Some(path.to_path_buf());
        self
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// A path the gate must prove absent, and the volume its record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsenceRequest {
    /// The consumer path the reference names.
    pub path: PathBuf,
    /// The APFS volume UUID the record holds for that path, when it holds one.
    pub volume: Option<VolumeUuid>,
}

/// One root's complete reference reading.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReferenceSet {
    /// The rows, in the order they were read.
    pub rows: Vec<ReferenceRow>,
    /// The paths whose absence the gate must prove.
    pub absence: Vec<AbsenceRequest>,
    /// Whether every source that could name this root was read completely.
    pub complete: bool,
    /// Why the set is incomplete, when it is.
    pub detail: Option<String>,
}

impl ReferenceSet {
    /// A complete set with no references: nothing names the root.
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            absence: Vec::new(),
            complete: true,
            detail: None,
        }
    }

    /// An incomplete set: some source could not be read, so the root is kept.
    pub fn unknown(detail: impl Into<String>) -> Self {
        Self {
            rows: Vec::new(),
            absence: Vec::new(),
            complete: false,
            detail: Some(detail.into()),
        }
    }

    /// Merge another reading of the same root into this one.
    ///
    /// Two incomplete readings keep both reasons: a reviewer needs to see every
    /// source that could not be read, not just the first.
    pub fn absorb(&mut self, other: Self) {
        self.rows.extend(other.rows);
        self.absence.extend(other.absence);
        if other.complete {
            return;
        }
        self.complete = false;
        match (self.detail.take(), other.detail) {
            (Some(seen), Some(detail)) if !seen.contains(&detail) => {
                self.detail = Some(format!("{seen}; {detail}"));
            }
            (Some(seen), _) => self.detail = Some(seen),
            (None, detail) => self.detail = detail,
        }
    }

    /// Whether any row is current.
    pub fn any_current(&self) -> bool {
        self.rows.iter().any(|row| row.current)
    }
}

/// The facts one release plan reads once about Barge's own placements.
#[derive(Debug, Clone)]
pub struct BargeReferences {
    placements: PlacementSet,
    placements_detail: Option<String>,
}

impl BargeReferences {
    /// Read every placement record once for one plan.
    pub fn read(state: &StatePaths) -> Self {
        #[cfg(not(test))]
        match cache::read_placements(state) {
            Ok(placements) => Self {
                placements_detail: placements.truncated.then(|| {
                    format!(
                        "the placement census holds more than {} records, so it was truncated",
                        placements.records.len()
                    )
                }),
                placements,
            },
            Err(error) => Self {
                placements: PlacementSet::empty(),
                placements_detail: Some(format!("the placement census could not be read: {error}")),
            },
        }
        #[cfg(test)]
        match cache::read_placements(state) {
            Ok(placements) => {
                #[cfg(test)]
                crate::tape::listing(
                    "reference::BargeReferences::read",
                    &state.placements_dir(),
                    placements.records.len(),
                    placements.records.len(),
                    placements.truncated,
                );
                Self {
                    placements_detail: placements.truncated.then(|| {
                        format!(
                            "the placement census holds more than {} records, so it was truncated",
                            placements.records.len()
                        )
                    }),
                    placements,
                }
            }
            Err(error) => {
                #[cfg(test)]
                crate::tape::listing_refused(
                    "reference::BargeReferences::read",
                    &state.placements_dir(),
                    error.kind().as_str(),
                );
                Self {
                    placements: PlacementSet::empty(),
                    placements_detail: Some(format!(
                        "the placement census could not be read: {error}"
                    )),
                }
            }
        }
    }

    /// One root's references from Barge's own records.
    ///
    /// `declaration` is the area declaration the plan was built under and
    /// `policy_permits_release` says whether that declaration's prune policy
    /// allows a root past its age to be released at all. `holds` is the root's hold
    /// reading, which the caller takes under the root's own lock: this function
    /// reads no state directory of its own, so a preview cannot observe a root
    /// outside its lock.
    pub fn references_for(
        &self,
        declaration: &AreaDocument,
        policy_permits_release: bool,
        root: RootRef<'_>,
        holds: &area::HoldReading,
        holds_error: Option<&str>,
    ) -> ReferenceSet {
        let mut set = ReferenceSet::empty();
        if let Some(detail) = &self.placements_detail {
            set.absorb(ReferenceSet::unknown(detail.clone()));
        }
        for record in &self.placements.records {
            if !placement_names_root(record, root) {
                continue;
            }
            set.rows.push(
                ReferenceRow::new(BARGE_PLACEMENT, true).with_detail(format!(
                    "placement {} of kind {} records `{}`, which is this root",
                    record.placement_id,
                    record.kind.as_str(),
                    record.recorded_path.display()
                )),
            );
        }
        set.rows.push(
            ReferenceRow::new(BARGE_AREA, !policy_permits_release).with_detail(format!(
                "the declaration {}/{} {}",
                declaration.namespace,
                declaration.area,
                if policy_permits_release {
                    "permits releasing a root, by its declared age or by reference"
                } else {
                    "declares a prune policy that never releases a root"
                }
            )),
        );
        if let Some(reason) = holds_error {
            set.absorb(ReferenceSet::unknown(format!(
                "the root's holds could not be read: {reason}"
            )));
        }
        set.absorb(hold_reference(holds));
        set
    }
}

/// Whether one placement record names this root.
///
/// A `held-root` or `link` placement is a placement of the root itself; a
/// `setting` placement names the consumer directory the setting points at. In
/// every form the recorded path is compared, and the recorded root ids are
/// compared when the form carries one.
fn placement_names_root(record: &cache::PlacementRecord, root: RootRef<'_>) -> bool {
    let recorded = &record.recorded_path;
    if recorded == root.path
        || recorded.starts_with(root.path)
        || root.path.starts_with(recorded)
        || records_root(recorded, root.path, root.mount_relative_path)
    {
        return true;
    }
    match &record.form {
        PlacementForm::HeldRoot(form) => form.held_root.root_id == root.id,
        PlacementForm::Link(form) => form.root_id == root.id,
        PlacementForm::Setting(form) => form.data_path == root.path,
    }
}

/// One hold reading as one reference row.
///
/// A live or undecidable hold is current; a positively stale hold is not, because
/// custody has already decided the holder is gone.
fn hold_reference(reading: &HoldReading) -> ReferenceSet {
    let mut set = ReferenceSet::empty();
    if reading.truncated {
        set.absorb(ReferenceSet::unknown(format!(
            "the root holds more than {} hold records",
            reading.holds.len()
        )));
    }
    let live = reading
        .holds
        .iter()
        .filter(|(_, state, _)| matches!(state, area::HoldState::Live | area::HoldState::Unknown))
        .count();
    let stale = reading.holds.len().saturating_sub(live);
    set.rows
        .push(ReferenceRow::new(BARGE_HOLD, live > 0).with_detail(format!(
            "{live} live or undecidable hold(s) and {stale} positively stale hold(s)"
        )));
    set
}

/// Whether one declaration's prune policy can release a root at all.
///
/// Only an age policy can, because the age is what this reader's rows decide.
/// `release_on_reference` permits a release too, but by reference, and the
/// effect that reads that permission is not part of this build: a reference row
/// that called it permission here would report a release nothing may perform.
/// The slice that adds the release-by-reference effect changes this reading.
pub const fn policy_permits_release(policy: &PrunePolicy) -> bool {
    matches!(policy, PrunePolicy::OldestIdleAfter { .. })
}

/// One `oxide.outputs.registry/2` (or `/1`) row, as this reader uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideRow {
    /// The checkout id the row belongs to.
    pub checkout_id: String,
    /// The checkout path the row belongs to.
    pub checkout: PathBuf,
    /// The checkout's APFS volume UUID, when the row records one.
    pub checkout_volume: Option<VolumeUuid>,
    /// The root the row names.
    pub root: PathBuf,
    /// The root's immutable nonce, when the row records one.
    pub nonce: Option<String>,
    /// Whether the row still selects its root.
    pub selected: bool,
    /// Whether a commitment is pending for the row.
    pub pending: bool,
    /// The intent that is pruning the root, when one is.
    pub pruning: bool,
    /// The reservation transaction that created the row, when one did.
    pub reservation: bool,
    /// The cancellation transaction that is dropping the row, when one is.
    pub cancelling: bool,
}

impl OxideRow {
    /// Oxide's own `Row::live`: a row that is not settled names its root.
    pub const fn live(&self) -> bool {
        self.selected || self.pending || self.pruning || self.reservation || self.cancelling
    }

    fn flags(&self) -> String {
        let mut flags = Vec::new();
        for (name, set) in [
            ("selected", self.selected),
            ("pending", self.pending),
            ("pruning", self.pruning),
            ("reservation", self.reservation),
            ("cancelling", self.cancelling),
        ] {
            if set {
                flags.push(name);
            }
        }
        if flags.is_empty() {
            "settled".to_owned()
        } else {
            flags.join(", ")
        }
    }
}

/// One unfinished prune intent from Oxide's registry journal, with the identity
/// facts the adapter binds.
///
/// The extra fields carry what the document already stated, through the same
/// strict parse that produced the reference rows: nothing is re-read, defaulted
/// or reinterpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideIntent {
    /// The root the intent names.
    pub root: PathBuf,
    /// The journal phase it records.
    pub phase: String,
    /// The intent's id: Oxide's 128-bit lowercase-hex nonce.
    pub id: String,
    /// The quarantine leaf the root was renamed to.
    pub quarantine: PathBuf,
    /// The preview digest that authorised the move.
    pub preview_sha256: String,
    /// The intent's pinned root identity.
    pub identity: OxideIntentIdentity,
    /// The explicit schema-3 route; historical formats carry no inferred route.
    pub deletion_route: Option<DeletionRoute>,
}

/// A validated schema-3 deletion route. This is routing evidence, not authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletionRoute {
    /// Oxide owns this operation; the Barge adapter must refuse it.
    Local,
    /// Barge owns deletion, bound to immutable original-request metadata.
    Barge {
        /// SHA-256 of the exact request-sidecar bytes, not the preview or request digest.
        request_sha256: String,
    },
}

/// One intent's pinned root identity, as its document records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideIntentIdentity {
    /// The checkout the root belonged to.
    pub checkout_id: String,
    /// The checkout path.
    pub checkout: PathBuf,
    /// The receipt path.
    pub receipt: PathBuf,
    /// The member the root served.
    pub member: String,
    /// The root path.
    pub root: PathBuf,
    /// The root kind Oxide records.
    pub root_kind: String,
    /// The root's volume UUID, when the identity records one.
    pub volume_uuid: Option<String>,
    /// The root's immutable nonce, when the identity records one.
    pub nonce: Option<String>,
    /// The root-creation generation.
    pub generation: u64,
    /// The device observed at intent time (never a durable identity).
    pub dev: u64,
    /// The inode the identity binds.
    pub ino: u64,
    /// The marker bytes' digest the identity binds.
    pub marker_digest: String,
}

/// The registry, as one reference snapshot read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OxideRegistry {
    /// No registry file is present: Oxide has never written one here.
    Absent,
    /// A registry that was read strictly and completely.
    Read {
        /// Its rows, bounded.
        rows: Vec<OxideRow>,
        /// The unfinished prune intents it journals.
        intents: Vec<OxideIntent>,
        /// Its own `complete` flag.
        complete: bool,
        /// The fence version the document states.
        version: String,
        /// The initialization transaction that created this registry.
        created_by: String,
    },
    /// A registry that could not be read completely. Every root it might name is
    /// kept.
    Unknown {
        /// Why, for a human.
        reason: String,
    },
}

/// One root marker's ownership, as this reader uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideMarker {
    /// The checkout the marker names.
    pub checkout: PathBuf,
    /// The checkout's id.
    pub checkout_id: String,
    /// The checkout's APFS volume UUID, when the marker records one.
    pub checkout_volume: Option<VolumeUuid>,
}

/// One receipt, reduced to the references it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideReceiptReference {
    /// The receipt's file name, which is its checkout id.
    pub checkout_id: String,
    /// The checkout path the receipt names.
    pub checkout: PathBuf,
    /// Every root the receipt selects, prepares, reserves or names as history.
    pub roots: Vec<PathBuf>,
    /// The roots the receipt currently selects, prepares or reserves.
    pub current: Vec<PathBuf>,
}

/// Whether one receipt was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OxideReceipts {
    /// No receipts folder is present.
    Absent,
    /// Every receipt that is present was read.
    Read(Vec<OxideReceiptReference>),
    /// At least one receipt could not be read. Every root any receipt might name
    /// is kept.
    Unknown {
        /// Why, for a human.
        reason: String,
    },
}

/// One plan's Oxide reference snapshot. Read once and never cached across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideSnapshot {
    /// The control directory the snapshot was read from, when one was found.
    pub control: Option<PathBuf>,
    /// The registry reading.
    pub registry: OxideRegistry,
    /// The receipt reading.
    pub receipts: OxideReceipts,
    /// Why the snapshot as a whole is incomplete, when it is.
    pub detail: Option<String>,
    /// The writer custody of the pools that hold these outputs, read once.
    pub custody: PoolCustody,
}

impl OxideSnapshot {
    /// No Oxide outputs control directory: no Oxide record can name a root.
    pub fn none() -> Self {
        Self {
            control: None,
            registry: OxideRegistry::Absent,
            receipts: OxideReceipts::Absent,
            detail: None,
            custody: PoolCustody::default(),
        }
    }

    /// Whether the whole snapshot was read.
    ///
    /// A registry that was read but reports itself incomplete is not a complete
    /// snapshot: Oxide's own flag says a row may be missing, so no root it might
    /// name is released.
    pub fn complete(&self) -> bool {
        let registry_complete = match &self.registry {
            OxideRegistry::Absent => true,
            OxideRegistry::Read { complete, .. } => *complete,
            OxideRegistry::Unknown { .. } => false,
        };
        registry_complete
            && !matches!(self.receipts, OxideReceipts::Unknown { .. })
            && self.custody.unattributed().is_none()
    }

    /// Read Oxide's reference records for this account, bounded and read-only.
    ///
    /// The control directory is found through Oxide's own pools record and its
    /// current and retired default pools, exactly as S2's observations are. A
    /// pool whose control directory holds an `outputs/` folder is the one this
    /// snapshot reads; nothing is created or repaired.
    pub fn read(account: &dyn AccountFacts) -> Self {
        Self::read_within(account, REFERENCE_WALL)
    }

    /// [`OxideSnapshot::read`] with an explicit wall clock, so a test can prove what
    /// an observation that reached its own bound reports.
    ///
    /// The one deadline covers discovery *and* the registry and receipts: finding
    /// Oxide's control directory is part of the observation, so time spent there is
    /// time the bound has already spent. Nothing is concluded outside it — not even
    /// "there is no control directory" or "the receipts folder is absent", because a
    /// look that ran out of time did not see the whole world.
    pub fn read_within(account: &dyn AccountFacts, wall: Duration) -> Self {
        let deadline = Deadline::after(wall);
        let control = match control_directory(account, deadline) {
            Ok(Some(control)) => control,
            Ok(None) => return Self::none(),
            Err(reason) => return Self::unread(reason.to_string()),
        };
        let outputs = control.join(OXIDE_OUTPUTS_DIR);
        let registry = read_registry(&outputs.join(OXIDE_REGISTRY_FILE), deadline);
        let receipts = read_receipts(&outputs, deadline);
        let custody = match pools_holding(&control, deadline) {
            Ok(pools) => PoolCustody::read(&pools, deadline),
            Err(reason) => PoolCustody::unknown(reason),
        };
        // The coordinated control pair and the `/4` registry image are one
        // handoff: neither decides anything without the other.
        let registry = bind_coordinated_registry(registry, custody.coordinated());
        let detail = match (&registry, &receipts) {
            (OxideRegistry::Unknown { reason }, _) => Some(reason.clone()),
            (_, OxideReceipts::Unknown { reason }) => Some(reason.clone()),
            _ => custody
                .unattributed()
                .map(|reason| format!("Oxide's pool custody is unknown: {reason}")),
        };
        Self {
            control: Some(control),
            registry,
            receipts,
            detail,
            custody,
        }
    }

    /// A snapshot the caller did not read, because the run's own wall clock was
    /// already spent: unknown, never empty.
    pub fn skipped(reason: String) -> Self {
        Self::unread(reason)
    }

    /// A snapshot that could not be read at all: unknown, never empty.
    fn unread(reason: String) -> Self {
        Self {
            control: None,
            registry: OxideRegistry::Unknown {
                reason: reason.clone(),
            },
            receipts: OxideReceipts::Unknown {
                reason: reason.clone(),
            },
            detail: Some(reason),
            custody: PoolCustody::default(),
        }
    }

    /// Every reference in this snapshot that names one root.
    ///
    /// `marker` is the root's own Oxide marker when it carries one. An Oxide
    /// root whose receipt cannot be found is an incomplete set: ownership
    /// without its selection record is exactly the case that must not be read as
    /// "nothing names it".
    pub fn references_for(
        &self,
        root_path: &Path,
        mount_relative_path: &str,
        marker: Option<&OxideMarker>,
        marker_unknown: Option<&str>,
    ) -> ReferenceSet {
        let mut set = ReferenceSet::empty();
        if let Some(detail) = &self.detail {
            set.absorb(ReferenceSet::unknown(detail.clone()));
        }
        if let Some(reason) = marker_unknown {
            set.absorb(ReferenceSet::unknown(format!(
                "the root's Oxide marker is unreadable: {reason}"
            )));
        }
        let uuid = marker.and_then(|marker| marker.checkout_volume.clone());
        // The checkouts whose own records still select this root. A marker's
        // reference is current only when its own checkout is one of them.
        let mut selecting: Vec<PathBuf> = Vec::new();
        // Every checkout that names this root, whose writers must be closed first.
        let mut named: Vec<PathBuf> = Vec::new();
        match &self.registry {
            OxideRegistry::Absent => {}
            OxideRegistry::Unknown { reason } => {
                set.absorb(ReferenceSet::unknown(format!(
                    "Oxide's outputs registry is unreadable: {reason}"
                )));
            }
            OxideRegistry::Read {
                rows,
                intents,
                complete,
                ..
            } => {
                if !complete {
                    set.absorb(ReferenceSet::unknown(
                        "Oxide's outputs registry reports itself incomplete",
                    ));
                }
                for row in rows
                    .iter()
                    .filter(|row| row_names_root(row, root_path, mount_relative_path))
                {
                    let row_volume = row.checkout_volume.clone();
                    named.push(row.checkout.clone());
                    let reference =
                        ReferenceRow::new(OXIDE_REGISTRY, row.live()).with_detail(format!(
                            "the registry row for checkout {} is {}",
                            row.checkout.display(),
                            row.flags()
                        ));
                    if row.live() {
                        selecting.push(row.checkout.clone());
                        set.rows.push(reference);
                    } else {
                        set.rows.push(reference.with_path(&row.checkout));
                        set.absence.push(AbsenceRequest {
                            path: row.checkout.clone(),
                            volume: row_volume,
                        });
                    }
                }
                for intent in intents
                    .iter()
                    .filter(|intent| records_root(&intent.root, root_path, mount_relative_path))
                {
                    set.rows
                        .push(ReferenceRow::new(OXIDE_REGISTRY, true).with_detail(format!(
                            "an unfinished prune intent (phase `{}`) names this root",
                            intent.phase
                        )));
                }
            }
        }
        match &self.receipts {
            OxideReceipts::Absent => {}
            OxideReceipts::Unknown { reason } => {
                set.absorb(ReferenceSet::unknown(format!(
                    "Oxide's receipts are unreadable: {reason}"
                )));
            }
            OxideReceipts::Read(receipts) => {
                for receipt in receipts {
                    if !receipt
                        .roots
                        .iter()
                        .any(|root| records_root(root, root_path, mount_relative_path))
                    {
                        continue;
                    }
                    named.push(receipt.checkout.clone());
                    let current = receipt
                        .current
                        .iter()
                        .any(|root| records_root(root, root_path, mount_relative_path));
                    if current {
                        selecting.push(receipt.checkout.clone());
                    }
                    let reference = ReferenceRow::new(OXIDE_RECEIPT, current).with_detail(format!(
                        "checkout {}'s receipt (id {}) names this root as {}",
                        receipt.checkout.display(),
                        receipt.checkout_id,
                        if current {
                            "a current selection, prepared root or reservation"
                        } else {
                            "history only"
                        }
                    ));
                    if current {
                        set.rows.push(reference);
                    } else {
                        set.rows.push(reference.with_path(&receipt.checkout));
                        set.absence.push(AbsenceRequest {
                            path: receipt.checkout.clone(),
                            volume: uuid.clone(),
                        });
                    }
                }
            }
        }
        if let Some(marker) = marker {
            named.push(marker.checkout.clone());
            // A marker names the checkout that owns the root. It is not a
            // selection, so a marker-only reference is never current on its own:
            // its own rule is "the consumer path is gone", and the gate must prove
            // that path absent before the root can be released. Without the proof
            // the row is simply unknown, which keeps the root.
            let selected_here = selecting
                .iter()
                .any(|checkout| checkout == &marker.checkout);
            if selected_here {
                set.rows
                    .push(ReferenceRow::new(OXIDE_MARKER, true).with_detail(format!(
                        "the root's Oxide marker names checkout {} (id {}), which still selects it",
                        marker.checkout.display(),
                        marker.checkout_id
                    )));
            } else {
                set.rows.push(
                    ReferenceRow::new(OXIDE_MARKER, false)
                        .with_path(&marker.checkout)
                        .with_detail(format!(
                            "the root's Oxide marker names checkout {} (id {}); its consumer path must be proved absent",
                            marker.checkout.display(),
                            marker.checkout_id
                        )),
                );
                set.absence.push(AbsenceRequest {
                    path: marker.checkout.clone(),
                    volume: marker.checkout_volume.clone(),
                });
            }
            // Ownership has to agree. A record that carries the marker's checkout
            // id but names another checkout path contradicts the marker, and which
            // checkout owns this root is then unknown rather than settled.
            match &self.receipts {
                OxideReceipts::Read(receipts) => {
                    for receipt in receipts
                        .iter()
                        .filter(|receipt| receipt.checkout_id == marker.checkout_id)
                        .filter(|receipt| receipt.checkout != marker.checkout)
                    {
                        set.absorb(ReferenceSet::unknown(format!(
                            "the receipt for checkout id {} names checkout {}, but the root's marker names {}",
                            marker.checkout_id,
                            receipt.checkout.display(),
                            marker.checkout.display()
                        )));
                    }
                }
                OxideReceipts::Absent | OxideReceipts::Unknown { .. } => {}
            }
            if let OxideRegistry::Read { rows, .. } = &self.registry {
                for row in rows
                    .iter()
                    .filter(|row| row.checkout_id == marker.checkout_id)
                    .filter(|row| row.checkout != marker.checkout)
                {
                    set.absorb(ReferenceSet::unknown(format!(
                        "the registry row for checkout id {} names checkout {}, but the root's marker names {}",
                        marker.checkout_id,
                        row.checkout.display(),
                        marker.checkout.display()
                    )));
                }
            }
            let receipt_present = match &self.receipts {
                OxideReceipts::Absent => false,
                OxideReceipts::Read(receipts) => receipts
                    .iter()
                    .any(|receipt| receipt.checkout_id == marker.checkout_id),
                // An unreadable receipt set is already incomplete.
                OxideReceipts::Unknown { .. } => true,
            };
            if !receipt_present {
                set.absorb(ReferenceSet::unknown(format!(
                    "the receipt for checkout id {} that owns this root is not present",
                    marker.checkout_id
                )));
            }
        }
        named.sort();
        named.dedup();
        for checkout in &named {
            match self.custody.for_checkout(checkout) {
                Custody::Closed => {}
                Custody::Held(detail) => {
                    set.rows
                        .push(
                            ReferenceRow::new(OXIDE_WRITER_CUSTODY, true).with_detail(format!(
                                "checkout {} has a writer still held: {detail}",
                                checkout.display()
                            )),
                        )
                }
                Custody::Unknown(detail) => set.absorb(ReferenceSet::unknown(format!(
                    "the writer custody of checkout {} is unknown: {detail}",
                    checkout.display()
                ))),
            }
        }
        set
    }
}

/// The facts that identify one planned root to the reference readers.
///
/// They are the root's id and its two paths: the absolute one this plan observed and
/// the mount-relative one that survives a remount (stage 4, "Root identity").
#[derive(Debug, Clone, Copy)]
pub struct RootRef<'a> {
    /// The root's id, which is its marker's id or its directory's name.
    pub id: &'a str,
    /// The absolute path this plan observed.
    pub path: &'a Path,
    /// The path below the verified mount root.
    pub mount_relative_path: &'a str,
}

/// Whether one recorded root path names one planned root.
///
/// A reference recorded before a mount-point change carries the *old* absolute path,
/// while the root itself is remount-stable: the mount-relative path is the identity
/// and the absolute path is only an observation (stage 4, "Root identity"). So a
/// record names this root when its path is the same path, or when it sits below a
/// `/Volumes/<mount>` prefix and the components below that prefix are exactly the
/// planned root's mount-relative path. The second form is what makes a current
/// Oxide reference survive a reconnect at another mount name; matching it keeps the
/// root, which is the safe direction. A path outside the mount parent, or one whose
/// relative path differs, names no root here.
pub fn records_root(recorded: &Path, root_path: &Path, mount_relative_path: &str) -> bool {
    if recorded == root_path {
        return true;
    }
    let Some(relative) = recorded.strip_prefix("/Volumes").ok().and_then(|below| {
        // `/Volumes/<mount>/…`: the mount name is one component.
        let mut components = below.components();
        components.next()?;
        let rest: PathBuf = components.collect();
        Some(rest)
    }) else {
        return false;
    };
    let planned: PathBuf = mount_relative_path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    !planned.as_os_str().is_empty() && relative == planned
}

/// Whether one registry row names one root.
///
/// The recorded root path is the key, read remount-aware (see [`records_root`]). The
/// row's nonce is carried as evidence and is never compared: a Barge-owned root's
/// nonce is Barge's, an Oxide marker's is Oxide's, so comparing them would decide a
/// fact neither record states. Path matching authorises nothing on its own either —
/// it only makes a reference row current, which keeps the root.
fn row_names_root(row: &OxideRow, root_path: &Path, mount_relative_path: &str) -> bool {
    records_root(&row.root, root_path, mount_relative_path)
}

/// Find Oxide's control directory: the one whose `outputs/` folder exists.
///
/// `Ok(None)` means every pool Oxide names, and every default pool, was looked at
/// and none holds an `outputs/` folder: no Oxide record can name a root here.
/// Anything that makes "every pool" uncertain is an error, never an empty
/// answer — Oxide's pools record may name the only pool that has outputs, so
/// reading an unreadable record as "no pools" would licence a release it should
/// keep. Two control directories that both hold outputs are ambiguous for the
/// same reason and are refused rather than picked between.
pub(crate) fn control_directory(
    account: &dyn AccountFacts,
    deadline: Deadline,
) -> Result<Option<PathBuf>> {
    control_directory_with(account, deadline, &inspect_outputs)
}

/// One candidate's `outputs` path, as the platform reports it.
fn inspect_outputs(path: &Path) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

/// How a candidate's `outputs` path is inspected; it is a parameter so a test can
/// make one lookup consume the bound.
type OutputsInspect<'a> = &'a dyn Fn(&Path) -> std::io::Result<std::fs::Metadata>;

/// [`control_directory`] with its candidate lookup injected.
fn control_directory_with(
    account: &dyn AccountFacts,
    deadline: Deadline,
    inspect: OutputsInspect<'_>,
) -> Result<Option<PathBuf>> {
    if deadline.expired() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "finding Oxide's control directory did not finish inside its {REFERENCE_WALL:?} bound"
            ),
        ));
    }
    let state_dir = account.tool_state_dir(crate::inspect::OXIDE_TOOL)?;
    let cache_dir = account.tool_cache_dir(crate::inspect::OXIDE_TOOL)?;
    let observations = crate::inspect::observe_oxide_at(&state_dir, &cache_dir);
    if deadline.expired() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "finding Oxide's control directory did not finish inside its {REFERENCE_WALL:?} bound"
            ),
        ));
    }
    if let crate::inspect::Observation::Unknown {
        path,
        detail,
        source,
    } = &observations.pools
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "Oxide's pools record at {} cannot be read as `{source}`, so the pools that could hold its outputs are unknown: {detail}",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for registration in &observations.searched {
        // A registration path is `<pool>/oxide-managed/removable.json`, so the
        // control directory is its parent.
        let Some(control) = registration.parent() else {
            continue;
        };
        let outputs = control.join(OXIDE_OUTPUTS_DIR);
        let found_here = inspect(&outputs);
        #[cfg(test)]
        crate::tape::io_stat("reference::control_directory_with", &outputs, &found_here);
        // A candidate lookup can consume the bound by itself: an expired observation
        // never concludes "no control directory exists", it is unknown.
        if deadline.expired() {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "inspecting a candidate control directory did not finish inside the {REFERENCE_WALL:?} bound"
                ),
            ));
        }
        match found_here {
            Ok(metadata) if metadata.is_dir() => {
                if !found.iter().any(|seen| seen == control) {
                    found.push(control.to_path_buf());
                }
            }
            Ok(_) => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "{} exists and is not a directory, so Oxide's outputs cannot be read",
                        outputs.display()
                    ),
                )
                .with_detail("path", outputs.display().to_string().into()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // An absent `outputs` folder is only "no registry here" for a
                // pool that never opted into owned outputs. A control state that
                // records a covered fence always has (or must have) its registry
                // image beside it, so the absence is unknown, never an empty
                // reference set — and a state that cannot be read is unknown too.
                if deadline.expired() {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        format!(
                            "inspecting a candidate control directory did not finish inside the {REFERENCE_WALL:?} bound"
                        ),
                    ));
                }
                match crate::pool_custody::covered_outputs_state(control, deadline) {
                    Ok(false) => continue,
                    Ok(true) => {
                        return Err(Error::new(
                            ErrorKind::UnknownState,
                            format!(
                                "{} records a covered owned-outputs fence but {} is absent, so its registry image cannot be read",
                                control.display(),
                                outputs.display()
                            ),
                        )
                        .with_detail("path", outputs.display().to_string().into()))
                    }
                    Err(reason) => {
                        return Err(Error::new(
                            ErrorKind::UnknownState,
                            format!(
                                "{} cannot be read while {} is absent: {reason}",
                                control.display(),
                                outputs.display()
                            ),
                        )
                        .with_detail("path", control.display().to_string().into()))
                    }
                }
            }
            Err(error) => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "{} cannot be inspected, so whether Oxide's outputs live there is unknown: {error}",
                        outputs.display()
                    ),
                )
                .with_detail("path", outputs.display().to_string().into()))
            }
        }
    }
    // The last lookup can also have consumed the bound, so the "nothing was found"
    // conclusion is drawn only inside it.
    if deadline.expired() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "finding Oxide's control directory did not finish inside its {REFERENCE_WALL:?} bound"
            ),
        ));
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} Oxide control directories hold an `outputs` folder, so which one is Oxide's is unknown",
                found.len()
            ),
        )
        .with_detail(
            "paths",
            found
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<String>>()
                .into(),
        )),
    }
}

/// The pool directories whose writers can name Oxide's outputs under one control
/// directory: the internal pool, and the removable pool whenever one is either
/// registered or still present on disk.
///
/// Two facts decide the peer, and both are conservative:
///
/// - **Registration** is the coordinated pair membership, exactly as Oxide's own
///   `removable::coordinate` decides it: a registered pool's control is the
///   sibling `<internal control>/removable/oxide-managed`, kept inside the
///   internal control directory so it stays readable while the volume is away.
///   The peer is therefore named even when its own directory is absent, so a
///   registered pair can never be silently narrowed to one control.
/// - **An extant `removable` entry** is a peer for custody's sake whatever the
///   registration says: a historical product kept its control state and ledger
///   there, and its writers still hold their checkouts. Dropping the entry would
///   read those writers as closed. An entry that is not a real directory is
///   unknown, never skipped.
///
/// A registration that cannot be read as Oxide's is unknown too.
pub(crate) fn pools_holding(
    control: &Path,
    deadline: Deadline,
) -> std::result::Result<Vec<PathBuf>, String> {
    let Some(internal) = control.parent() else {
        return Err(format!(
            "{} has no pool directory above it, so its writers are unknown",
            control.display()
        ));
    };
    let registration = control.join(crate::inspect::OXIDE_REGISTRATION_FILE);
    let removable = control.join(REMOVABLE_POOL_DIR);
    let expired = || {
        format!(
            "the read deadline expired while {} was inspected, so the pools that could hold Oxide's outputs are unknown",
            registration.display()
        )
    };
    if deadline.expired() {
        return Err(expired());
    }
    // The identity row is this site's own observation, and the reader below is
    // the one strict registration reader the census already uses.
    #[cfg(test)]
    crate::tape::stat(
        "reference::pools_holding",
        &registration,
        std::fs::symlink_metadata(&registration)
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino(), "registration")),
    );
    let registered = match crate::inspect::read_registration(&registration) {
        crate::inspect::Observation::Absent { .. } => false,
        crate::inspect::Observation::Reported(_) => true,
        crate::inspect::Observation::Unknown { detail, .. } => {
            return Err(format!(
                "{} cannot be read as Oxide's pool registration, so the pools that could hold its outputs are unknown: {detail}",
                registration.display()
            ))
        }
    };
    if deadline.expired() {
        return Err(expired());
    }
    // The peer entry's own identity, read even when nothing registers it: a
    // legacy pool's writers are custody facts, not a registration claim.
    let entry = match std::fs::symlink_metadata(&removable) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "{} cannot be inspected, so the removable pool's writers are unknown: {error}",
                removable.display()
            ))
        }
    };
    #[cfg(test)]
    crate::tape::stat(
        "reference::pools_holding",
        &removable,
        entry
            .as_ref()
            .map(|metadata| (metadata.dev(), metadata.ino(), "removable-entry")),
    );
    match entry {
        Some(metadata) if !metadata.is_dir() => Err(format!(
            "{} exists and is not a directory, so the removable pool's writers are unknown",
            removable.display()
        )),
        Some(_) => Ok(vec![internal.to_path_buf(), removable]),
        None if registered => Ok(vec![internal.to_path_buf(), removable]),
        None => Ok(vec![internal.to_path_buf()]),
    }
}

/// Read Oxide's outputs registry strictly.
pub(crate) fn read_registry(path: &Path, deadline: Deadline) -> OxideRegistry {
    let bounds = Bounds::new(REGISTRY_CAP_BYTES, REGISTRY_ROWS, deadline);
    #[cfg(not(test))]
    let bytes = match state::read_bounded(path, &bounds) {
        ReadOutcome::Absent => return OxideRegistry::Absent,
        ReadOutcome::Refused(error) => {
            return OxideRegistry::Unknown {
                reason: format!("{}: {error}", path.display()),
            }
        }
        ReadOutcome::Present(bytes) => bytes,
    };
    #[cfg(test)]
    let bytes = match state::read_bounded(path, &bounds) {
        ReadOutcome::Absent => {
            #[cfg(test)]
            crate::tape::read_absent("reference::read_registry", path);
            return OxideRegistry::Absent;
        }
        ReadOutcome::Refused(error) => {
            #[cfg(test)]
            crate::tape::read_refused("reference::read_registry", path, error.kind().as_str());
            return OxideRegistry::Unknown {
                reason: format!("{}: {error}", path.display()),
            };
        }
        ReadOutcome::Present(bytes) => {
            #[cfg(test)]
            crate::tape::read_present(
                "reference::read_registry",
                path,
                None,
                &bytes,
                Some(REGISTRY_CAP_BYTES as u64),
            );
            bytes
        }
    };
    #[cfg(not(test))]
    match parse_registry(&bytes) {
        Ok(reading) => reading,
        Err(error) => OxideRegistry::Unknown {
            reason: format!("{}: {error}", path.display()),
        },
    }
    #[cfg(test)]
    match parse_registry(&bytes) {
        Ok(reading) => reading,
        Err(error) => {
            #[cfg(test)]
            crate::tape::read_refused("reference::read_registry", path, "registry_parse");
            OxideRegistry::Unknown {
                reason: format!("{}: {error}", path.display()),
            }
        }
    }
}

/// Bind Oxide's coordinated control pair to the registry image it published.
///
/// The producer writes both pools' `oxide.outputs/5` fence and `initializing`
/// record under one transaction, publishes the atomic `/4` registry image
/// created by that transaction, and only then records `ready` on each control.
/// So a completed pair and a `/4` image exist together or not at all: an image
/// without the pair is one-sided adoption, and a pair whose transaction is not
/// the image's `created_by` is a mixed generation. Either way the registry is
/// unknown, and every root it might name is kept.
pub(crate) fn bind_coordinated_registry(
    registry: OxideRegistry,
    coordinated: Option<&CoordinatedAdoption>,
) -> OxideRegistry {
    let (version, created_by) = match &registry {
        OxideRegistry::Read {
            version,
            created_by,
            ..
        } => (Some(version.as_str()), Some(created_by.as_str())),
        OxideRegistry::Absent => (None, None),
        // An unreadable registry is already the reading's own unknown fact.
        OxideRegistry::Unknown { .. } => return registry,
    };
    let coordinated_image = version == Some(OXIDE_REGISTRY_V4);
    let reason = match (coordinated, coordinated_image) {
        (Some(adoption), true) if created_by == Some(adoption.transaction()) => return registry,
        (Some(adoption), true) => format!(
            "the registry image records transaction {:?}, not the completed coordinated transaction {:?}, so the paired adoption is mixed",
            created_by.unwrap_or_default(),
            adoption.transaction()
        ),
        (Some(_), false) => format!(
            "the pools record the completed coordinated adoption but the registry image is {}, not {OXIDE_REGISTRY_V4}",
            version.unwrap_or("absent")
        ),
        (None, true) => format!(
            "the registry image is {OXIDE_REGISTRY_V4} but no pool control records the completed coordinated fence that creates it"
        ),
        (None, false) => return registry,
    };
    OxideRegistry::Unknown { reason }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    version: String,
    #[serde(default)]
    schema: u32,
    // Oxide always writes the revision, the completeness flag, the creator and
    // its own incomplete record. A registry that omits one is a document this
    // build does not understand, and defaulting it would turn unknown reference
    // evidence into "no references".
    #[allow(dead_code)]
    revision: u64,
    complete: bool,
    #[serde(default)]
    incomplete: Option<RawIncomplete>,
    #[allow(dead_code)]
    created_by: String,
    // Oxide always writes its row array, even when it is empty, so a registry that
    // omits it is a document this build does not understand. Defaulting it would read
    // a missing reference set as "no references", which is exactly the unknown that
    // must keep every root it covers.
    rows: Vec<RawRow>,
    #[serde(default)]
    intents: Vec<RawIntent>,
    #[serde(default, deserialize_with = "read_routes")]
    deletion_routes: Option<Vec<RawDeletionRoute>>,
    #[serde(default)]
    #[allow(dead_code)]
    cancelled_overflow: Vec<String>,
    /// The coordinated registry's retained settlement facts, read and
    /// validated through the one shared rule both readers use. Required
    /// present on `/4`, refused on every predecessor.
    #[serde(
        default,
        deserialize_with = "crate::oxide_settlement::read_settlements"
    )]
    settled: Option<Vec<crate::oxide_settlement::RawSettlement>>,
}

/// An explicitly present route array must be an array, including when empty.
fn read_routes<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<RawDeletionRoute>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::deserialize(deserializer).map(Some)
}

/// The exact wire alternatives: no implicit local route or extra fields.
#[derive(Debug, Deserialize)]
#[serde(tag = "deleter", rename_all = "snake_case", deny_unknown_fields)]
enum RawDeletionRoute {
    Local {
        intent_id: String,
    },
    Barge {
        intent_id: String,
        request_sha256: String,
    },
}

/// Oxide's own record of why a registry is sticky-incomplete.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIncomplete {
    #[allow(dead_code)]
    reason: String,
    #[allow(dead_code)]
    since_ms: u64,
    #[allow(dead_code)]
    transaction: String,
}

/// One unfinished prune journal entry. Oxide's entry pins the whole root identity
/// of the move it is making, and this reader takes the two facts it needs from a
/// document that must otherwise be exactly the one Oxide writes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntent {
    version: String,
    #[allow(dead_code)]
    id: String,
    identity: RawIntentIdentity,
    phase: String,
    // Oxide pins all of these on every journal entry: the quarantine leaf it
    // moved the root to, the plan digest that authorized the move and both times.
    // A journal that omits one is a document this build does not understand, and
    // defaulting it would read an unfinished move as a settled one.
    #[allow(dead_code)]
    quarantine: PathBuf,
    #[allow(dead_code)]
    preview_sha256: String,
    #[allow(dead_code)]
    created_ms: u64,
    #[allow(dead_code)]
    updated_ms: u64,
    #[serde(default)]
    #[allow(dead_code)]
    resolved_ms: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    receipt_settled: bool,
    #[serde(default = "selected_by_default")]
    #[allow(dead_code)]
    was_selected: bool,
    #[serde(default)]
    #[allow(dead_code)]
    prior_receipt_sha: String,
    #[serde(default)]
    #[allow(dead_code)]
    group: Vec<RawGroupRow>,
    #[serde(default)]
    #[allow(dead_code)]
    expected_terminal_sha: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    terminal_ms: Option<u64>,
}

/// One reference of an intent's (checkout, root) group.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroupRow {
    #[allow(dead_code)]
    member: String,
    #[allow(dead_code)]
    generation: u64,
    #[serde(default)]
    #[allow(dead_code)]
    nonce: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    root_dev: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    root_ino: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    marker_digest: Option<String>,
    #[allow(dead_code)]
    selected: bool,
}

/// An intent's representative row is selected unless it says otherwise.
const fn selected_by_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntentIdentity {
    #[allow(dead_code)]
    checkout_id: String,
    #[allow(dead_code)]
    checkout: PathBuf,
    #[allow(dead_code)]
    receipt: PathBuf,
    #[allow(dead_code)]
    member: String,
    root: PathBuf,
    #[allow(dead_code)]
    root_kind: String,
    #[serde(default)]
    #[allow(dead_code)]
    volume_uuid: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    nonce: Option<String>,
    #[allow(dead_code)]
    generation: u64,
    #[allow(dead_code)]
    dev: u64,
    #[allow(dead_code)]
    ino: u64,
    #[allow(dead_code)]
    marker_digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    #[allow(dead_code)]
    checkout_id: String,
    checkout: PathBuf,
    #[serde(default)]
    checkout_volume_uuid: Option<String>,
    #[allow(dead_code)]
    receipt: PathBuf,
    #[allow(dead_code)]
    member: String,
    root: PathBuf,
    #[allow(dead_code)]
    root_kind: String,
    #[serde(default)]
    #[allow(dead_code)]
    volume_uuid: Option<String>,
    #[allow(dead_code)]
    recipe: String,
    #[serde(default)]
    nonce: Option<String>,
    #[allow(dead_code)]
    generation: u64,
    #[serde(default)]
    #[allow(dead_code)]
    root_dev: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    root_ino: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    marker_digest: Option<String>,
    // Oxide's selection flags are required: a row that does not state them is a
    // document this build does not understand, never a row that selects nothing.
    // Defaulting them to `false` would read a missing selection as a settled one
    // and could release a root Oxide still wants.
    selected: bool,
    pending: bool,
    #[serde(default)]
    pruning: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pruned_ms: Option<u64>,
    #[serde(default)]
    reservation: Option<String>,
    #[serde(default)]
    cancelling: Option<String>,
}

fn parse_registry(bytes: &[u8]) -> std::result::Result<OxideRegistry, String> {
    let raw: RawRegistry = serde_json::from_slice(bytes)
        .map_err(|error| format!("the registry is not the field set Oxide writes: {error}"))?;
    if ![
        OXIDE_REGISTRY_V1,
        OXIDE_REGISTRY_V2,
        OXIDE_REGISTRY_V3,
        OXIDE_REGISTRY_V4,
    ]
    .contains(&raw.version.as_str())
    {
        return Err(format!(
            "`{}` is not a version this build reads",
            raw.version
        ));
    }
    // The version string is the format authority; Oxide's numeric `schema`
    // duplicates it and is only cross-checked for the current major.
    if raw.version == OXIDE_REGISTRY_V2 && raw.schema != 0 && raw.schema != 2 {
        return Err(format!(
            "a `{OXIDE_REGISTRY_V2}` registry states format {}",
            raw.schema
        ));
    }
    // Oxide's own validate: a coordinated registry is only ever the exact
    // version/schema pair, and it always writes the numeric schema.
    if raw.version == OXIDE_REGISTRY_V4 && raw.schema != 4 {
        return Err(format!(
            "a `{OXIDE_REGISTRY_V4}` registry must state numeric schema 4, not {}",
            raw.schema
        ));
    }
    let routed = raw.version == OXIDE_REGISTRY_V3;
    let mut routes = std::collections::BTreeMap::new();
    if routed {
        if raw.schema != 3 {
            return Err("a schema-3 registry must state numeric schema 3".to_owned());
        }
        let entries = raw
            .deletion_routes
            .ok_or_else(|| "a schema-3 registry omits deletion_routes".to_owned())?;
        if raw.intents.len() > 256 || entries.len() != raw.intents.len() {
            return Err(
                "deletion routes must cover exactly the retained intents within the 256-intent cap"
                    .to_owned(),
            );
        }
        for entry in entries {
            let (id, route) = match entry {
                RawDeletionRoute::Local { intent_id } => (intent_id, DeletionRoute::Local),
                RawDeletionRoute::Barge {
                    intent_id,
                    request_sha256,
                } => {
                    if !is_hex64(&request_sha256) {
                        return Err(
                            "a Barge route has no lowercase SHA-256 request binding".to_owned()
                        );
                    }
                    (intent_id, DeletionRoute::Barge { request_sha256 })
                }
            };
            if !is_hex32(&id) || routes.insert(id, route).is_some() {
                return Err("a deletion route has an invalid or duplicate intent id".to_owned());
            }
        }
    } else if raw.deletion_routes.is_some() {
        if raw.version == OXIDE_REGISTRY_V4 {
            return Err("the coordinated registry cannot carry deletion_routes".to_owned());
        }
        return Err("a historical registry cannot carry deletion_routes".to_owned());
    }
    // The coordinated registry's settlement set: required present and
    // validated on `/4`, refused on every predecessor. The entries are
    // retained terminal facts, never references: they are validated here and
    // dropped, because the registry's current rows and intents still decide
    // everything a reference depends on.
    match (raw.version == OXIDE_REGISTRY_V4, raw.settled) {
        (true, Some(entries)) => crate::oxide_settlement::validate_settlement_set(&entries)?,
        (true, None) => {
            return Err("a coordinated registry omits its settlement set".to_owned());
        }
        (false, Some(_)) => {
            return Err("a predecessor registry cannot carry settlement facts".to_owned());
        }
        (false, None) => {}
    }
    if raw.rows.len() > REGISTRY_ROWS {
        return Err(format!(
            "the registry holds more than the {REGISTRY_ROWS} rows its own cap allows"
        ));
    }
    let mut rows = Vec::with_capacity(raw.rows.len());
    for row in raw.rows {
        let checkout_volume = match row.checkout_volume_uuid.as_deref() {
            Some(raw) => Some(VolumeUuid::parse(raw).map_err(|error| {
                format!("a registry row states a checkout volume that is not a UUID: {error}")
            })?),
            None => None,
        };
        check_root_kind("a registry row", &row.root_kind)?;
        // Oxide's own `Row::validate`: a row names a lowercase 64-hex checkout id, a
        // bounded member and recipe, three absolute paths, and never a pruning row
        // that is also selected or pending. A row that fails any of these is a
        // divergent document, not a reference that happens to match nothing.
        if !is_hex64(&row.checkout_id)
            || !is_text(&row.member)
            || !is_text(&row.recipe)
            || !is_safe_absolute(&row.root)
            || !row.checkout.is_absolute()
            || !row.receipt.is_absolute()
        {
            return Err("the output registry has an unusable row".to_owned());
        }
        if row.pruning.is_some() && (row.selected || row.pending) {
            return Err("the output registry row is pruning and still referenced".to_owned());
        }
        rows.push(OxideRow {
            checkout_id: row.checkout_id,
            checkout: row.checkout,
            checkout_volume,
            root: row.root,
            nonce: row.nonce,
            selected: row.selected,
            pending: row.pending,
            pruning: row.pruning.is_some(),
            reservation: row.reservation.is_some(),
            cancelling: row.cancelling.is_some(),
        });
    }
    // Oxide's own invariant: a registry that is not complete carries the record
    // of why, and one that is complete carries none. Either half alone is
    // contradictory.
    match (raw.complete, raw.incomplete.is_some()) {
        (false, true) | (true, false) => {}
        (false, false) => {
            return Err("an incomplete registry carries no incomplete record".to_owned())
        }
        (true, true) => {
            return Err("a complete registry carries an incomplete record".to_owned());
        }
    }
    let mut intents = Vec::new();
    for intent in raw.intents {
        // The journal entry states its own version, and a version this build does
        // not read is a divergence: reading it as complete would let an
        // unrecognised move count as a settled one.
        if intent.version != OXIDE_INTENT_V1 {
            return Err(format!(
                "a prune intent states `{}`, which is not the {OXIDE_INTENT_V1} version this build reads",
                intent.version
            ));
        }
        if !matches!(
            intent.phase.as_str(),
            "intent" | "renamed" | "deleting" | "marker_last" | "deleted" | "resolved"
        ) {
            return Err(format!(
                "a prune intent states phase `{}`, which Oxide never writes",
                intent.phase
            ));
        }
        check_root_kind("a prune intent", &intent.identity.root_kind)?;
        // Oxide's own `Intent::validate`, for the facts a reference depends on: a
        // lowercase 32-hex id, a lowercase 64-hex checkout id, absolute receipt and
        // quarantine paths, a 64-hex plan digest, an empty-or-64-hex preimage, a
        // terminal image only with its time, and a sorted unique usable group.
        let mut members: Vec<&str> = intent.group.iter().map(|row| row.member.as_str()).collect();
        let unsorted = {
            let sorted = {
                let mut sorted = members.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted
            };
            sorted != members
        };
        if !is_hex32(&intent.id)
            || !is_hex64(&intent.identity.checkout_id)
            || !intent.identity.receipt.is_absolute()
            || !is_hex64(&intent.preview_sha256)
            || (!intent.prior_receipt_sha.is_empty() && !is_hex64(&intent.prior_receipt_sha))
            || intent
                .expected_terminal_sha
                .as_deref()
                .is_some_and(|sha| !is_hex64(sha) || intent.terminal_ms.is_none())
            || unsorted
            || intent.group.iter().any(|row| !is_text(&row.member))
        {
            return Err("the output registry has an unusable prune intent".to_owned());
        }
        members.clear();
        if !intent.quarantine.is_absolute() {
            return Err("a prune intent names no usable quarantine leaf".to_owned());
        }
        if intent.preview_sha256.len() != 64
            || !intent
                .preview_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("a prune intent pins no 64-hex plan digest".to_owned());
        }
        if intent.identity.dev == 0
            || intent.identity.ino == 0
            || intent.identity.generation == 0
            || !is_safe_absolute(&intent.identity.root)
            || intent.identity.marker_digest.len() != 64
            || !intent
                .identity
                .marker_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("a prune intent names an unusable root identity for the move".to_owned());
        }
        let deletion_route = if routed {
            Some(
                routes
                    .remove(&intent.id)
                    .ok_or_else(|| "an intent has no unique matching deletion route".to_owned())?,
            )
        } else {
            None
        };
        intents.push(OxideIntent {
            deletion_route,
            root: intent.identity.root.clone(),
            phase: intent.phase,
            id: intent.id,
            quarantine: intent.quarantine,
            preview_sha256: intent.preview_sha256,
            identity: OxideIntentIdentity {
                checkout_id: intent.identity.checkout_id,
                checkout: intent.identity.checkout,
                receipt: intent.identity.receipt,
                member: intent.identity.member,
                root: intent.identity.root,
                root_kind: intent.identity.root_kind,
                volume_uuid: intent.identity.volume_uuid,
                nonce: intent.identity.nonce,
                generation: intent.identity.generation,
                dev: intent.identity.dev,
                ino: intent.identity.ino,
                marker_digest: intent.identity.marker_digest,
            },
        });
    }
    if !routes.is_empty() {
        return Err("deletion_routes names an intent outside the registry".to_owned());
    }
    if raw.revision == 0 && !rows.is_empty() {
        return Err("a non-empty registry states revision 0".to_owned());
    }
    Ok(OxideRegistry::Read {
        rows,
        intents,
        complete: raw.complete,
        version: raw.version,
        created_by: raw.created_by,
    })
}

/// Read every receipt present in Oxide's outputs folder.
///
/// Three outcomes are kept apart, because only one of them authorises "no receipt
/// names this root":
///
/// - the folder is **established absent** (`ENOENT`): Oxide has never written a
///   receipt here, so there is nothing to read;
/// - every listed receipt was **read completely**: the census is a complete look;
/// - anything else is **unknown** — a folder that exists and is not a directory,
///   a folder or entry that cannot be inspected, a receipt that vanished after it
///   was listed, a receipt that cannot be read, and a bound that stopped the walk.
///   A metadata error is never collapsed into "absent", and a listed name that is
///   gone by the time it is read means the census changed under the observation
///   rather than that it holds nothing.
fn read_receipts(outputs: &Path, deadline: Deadline) -> OxideReceipts {
    read_receipts_with(outputs, deadline, &read_receipt)
}

/// How one listed receipt is read: the receipt, `None` when the listed name is no
/// longer there, or the reason it could not be read.
type ReceiptRead<'a> =
    &'a dyn Fn(&Path, &str, Deadline) -> std::result::Result<Option<OxideReceiptReference>, String>;

/// [`read_receipts`] with its per-receipt read injected, so a test can prove what a
/// receipt that disappears after it was listed does.
fn read_receipts_with(
    outputs: &Path,
    deadline: Deadline,
    read_one: ReceiptRead<'_>,
) -> OxideReceipts {
    #[cfg(test)]
    let parent_facts;
    #[cfg(not(test))]
    let outputs_state = match std::fs::metadata(outputs) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => {
            return OxideReceipts::Unknown {
                reason: format!(
                    "{} exists and is not a directory, so Oxide's receipts cannot be read",
                    outputs.display()
                ),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return OxideReceipts::Unknown {
                reason: format!(
                    "{} cannot be inspected, so whether Oxide's receipts live there is unknown: {error}",
                    outputs.display()
                ),
            }
        }
    };
    #[cfg(test)]
    let outputs_state = match std::fs::metadata(outputs) {
        Ok(metadata) if metadata.is_dir() => {
            #[cfg(test)]
            {
                parent_facts = Some((metadata.dev(), metadata.ino()));
                crate::tape::stat(
                    "reference::read_receipts_with::precheck",
                    outputs,
                    Some((metadata.dev(), metadata.ino(), "directory")),
                );
            }
            true
        }
        Ok(metadata) => {
            #[cfg(test)]
            crate::tape::stat(
                "reference::read_receipts_with::precheck",
                outputs,
                Some((metadata.dev(), metadata.ino(), "other")),
            );
            return OxideReceipts::Unknown {
                reason: format!(
                    "{} exists and is not a directory, so Oxide's receipts cannot be read",
                    outputs.display()
                ),
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(test)]
            {
                parent_facts = None;
                crate::tape::stat("reference::read_receipts_with::precheck", outputs, None);
            }
            false
        }
        Err(error) => {
            #[cfg(test)]
            crate::tape::stat_refused(
                "reference::read_receipts_with::precheck",
                outputs,
                &crate::tape::errno_class(&error),
            );
            return OxideReceipts::Unknown {
                reason: format!(
                    "{} cannot be inspected, so whether Oxide's receipts live there is unknown: {error}",
                    outputs.display()
                ),
            };
        }
    };
    if !outputs_state {
        // "There is no receipts folder" is a complete look's conclusion: a look that
        // ran out of time did not establish it.
        if deadline.expired() {
            return OxideReceipts::Unknown {
                reason: format!(
                    "deciding whether Oxide's receipts folder exists did not finish inside its {REFERENCE_WALL:?} bound"
                ),
            };
        }
        return OxideReceipts::Absent;
    }
    // The bound is checked before anything is concluded, including an empty folder:
    // a census that reached its own deadline did not see the whole folder, so it is
    // unknown rather than a complete empty one.
    if deadline.expired() {
        return OxideReceipts::Unknown {
            reason: format!(
                "reading Oxide's receipts did not finish inside its {REFERENCE_WALL:?} bound"
            ),
        };
    }
    let listing = match std::fs::read_dir(outputs) {
        Ok(listing) => listing,
        Err(error) => {
            return OxideReceipts::Unknown {
                reason: format!(
                    "{}: the folder cannot be listed: {error}",
                    outputs.display()
                ),
            }
        }
    };
    let mut receipts = Vec::new();
    for entry in listing {
        if deadline.expired() {
            return OxideReceipts::Unknown {
                reason: format!(
                    "reading Oxide's receipts did not finish inside its {REFERENCE_WALL:?} bound"
                ),
            };
        }
        let Ok(entry) = entry else {
            return OxideReceipts::Unknown {
                reason: format!("{}: a receipt entry cannot be read", outputs.display()),
            };
        };
        let path = entry.path();
        #[cfg(test)]
        crate::tape::extracted_dir_entry(
            "reference::read_receipts_with",
            parent_facts,
            &path
                .file_name()
                .map_or_else(std::ffi::OsString::new, std::ffi::OsStr::to_os_string),
        );
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let Some(checkout_id) = name.strip_suffix(".json") else {
            continue;
        };
        if checkout_id == OXIDE_REGISTRY_FILE.trim_end_matches(".json") {
            continue;
        }
        if receipts.len() >= RECEIPT_ROWS {
            return OxideReceipts::Unknown {
                reason: format!(
                    "Oxide's outputs folder holds more than the {RECEIPT_ROWS} receipts this reader examines"
                ),
            };
        }
        match read_one(&path, checkout_id, deadline) {
            #[cfg(not(test))]
            Ok(Some(receipt)) => receipts.push(receipt),
            #[cfg(test)]
            Ok(Some(receipt)) => {
                crate::tape::verdict(
                    "reference::read_receipts_with",
                    crate::tape::Identity::file(&path, None),
                    "receipt_read",
                );
                receipts.push(receipt)
            }
            // The name was listed and is no longer there: the folder changed while
            // it was being read, so this is not a complete look.
            Ok(None) => {
                return OxideReceipts::Unknown {
                    reason: format!(
                        "{} was listed and is no longer there, so the receipt census changed while it was read",
                        path.display()
                    ),
                }
            }
            Err(reason) => return OxideReceipts::Unknown { reason },
        }
    }
    // The last entry can consume the bound: the census is only complete while it is
    // still inside its own wall clock.
    if deadline.expired() {
        return OxideReceipts::Unknown {
            reason: format!(
                "reading Oxide's receipts did not finish inside its {REFERENCE_WALL:?} bound"
            ),
        };
    }
    OxideReceipts::Read(receipts)
}

fn read_receipt(
    path: &Path,
    checkout_id: &str,
    deadline: Deadline,
) -> std::result::Result<Option<OxideReceiptReference>, String> {
    let bounds = Bounds::new(RECEIPT_CAP_BYTES, 1, deadline);
    let bytes = match state::read_bounded(path, &bounds) {
        // A listed receipt that is gone by the time it is read is the caller's
        // "changed under the observation", never "absent".
        ReadOutcome::Absent => return Ok(None),
        ReadOutcome::Refused(error) => {
            return Err(format!("{}: {error}", path.display()));
        }
        ReadOutcome::Present(bytes) => bytes,
    };
    parse_receipt(&bytes, checkout_id).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReceipt {
    version: String,
    checkout: PathBuf,
    #[allow(dead_code)]
    checkout_id: String,
    #[allow(dead_code)]
    generation: u64,
    // Oxide always writes every field below except the optional ones serde
    // defaults, and it always writes one of its own states. A document that omits
    // a required field or states a state this build does not know is a divergence,
    // not a receipt with defaulted facts.
    #[allow(dead_code)]
    state: String,
    #[serde(default)]
    layout: Option<String>,
    #[serde(default)]
    transition: Option<RawMigration>,
    #[serde(default)]
    links: Vec<RawPlacement>,
    #[serde(default)]
    reserved: Option<RawReservation>,
    #[serde(default)]
    selected: Option<RawSelected>,
    #[serde(default)]
    prepared: Option<RawPrepared>,
    #[serde(default)]
    roots: Vec<RawSelected>,
    #[serde(default)]
    #[allow(dead_code)]
    pruned: Option<serde_json::Value>,
    #[allow(dead_code)]
    updated_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlacement {
    #[allow(dead_code)]
    member: String,
    #[serde(default)]
    selected: Option<RawSelected>,
    #[serde(default)]
    prepared: Option<RawPrepared>,
    #[serde(default)]
    reserved: Option<RawReservation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelected {
    #[allow(dead_code)]
    kind: String,
    root: PathBuf,
    #[serde(default)]
    #[allow(dead_code)]
    volume_uuid: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    nonce: Option<String>,
    #[allow(dead_code)]
    generation: u64,
}

/// A reservation names a future root; it is not a selection.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReservation {
    root: PathBuf,
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    nonce: String,
    #[allow(dead_code)]
    kind: String,
    #[serde(default)]
    #[allow(dead_code)]
    volume_uuid: Option<String>,
    #[allow(dead_code)]
    generation: u64,
    #[allow(dead_code)]
    created_ms: u64,
    #[serde(default)]
    #[allow(dead_code)]
    overflow: Option<serde_json::Value>,
}

/// Oxide's migration journal, read strictly: it pins the old link, parent, marker
/// and receipt, so a document that omits or invents a field is divergent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMigration {
    generation: u64,
    phase: String,
    root: RawSelected,
    members: Vec<String>,
    link: PathBuf,
    // The eight fields below are pinned by every journal Oxide writes: the old
    // link's exact text and identity, the parent's identity, the registry revision
    // the journal was published at and both times. A journal that omits one is
    // divergent, never a journal with defaulted facts.
    #[allow(dead_code)]
    link_text: PathBuf,
    #[allow(dead_code)]
    link_dev: u64,
    #[allow(dead_code)]
    link_ino: u64,
    #[allow(dead_code)]
    parent_dev: u64,
    #[allow(dead_code)]
    parent_ino: u64,
    stage: PathBuf,
    backup: PathBuf,
    #[serde(default)]
    #[allow(dead_code)]
    stage_dev: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    stage_ino: Option<u64>,
    #[serde(default)]
    marker_pin: Option<RawMarkerPin>,
    #[serde(default)]
    #[allow(dead_code)]
    marker_backup: Option<PathBuf>,
    prior_receipt_sha: String,
    #[allow(dead_code)]
    registry_revision: u64,
    #[allow(dead_code)]
    created_ms: u64,
    #[allow(dead_code)]
    updated_ms: u64,
}

/// An old root marker's exact identity, so a rollback restores those bytes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMarkerPin {
    #[allow(dead_code)]
    dev: u64,
    #[allow(dead_code)]
    ino: u64,
    sha256: String,
}

/// Whether a migration journal is exactly the shape Oxide publishes for this
/// receipt. A journal is durable evidence of a move in progress, so an unknown
/// phase, a generation its phase cannot have, an empty or unsorted member set, a
/// relative path or a digest that is not a SHA-256 is a divergence rather than a
/// journal with defaulted facts.
fn validate_migration(
    journal: &RawMigration,
    checkout: &Path,
    receipt_generation: u64,
) -> std::result::Result<(), String> {
    if !matches!(
        journal.phase.as_str(),
        "prepared" | "staged" | "marker_upgraded" | "backed_up" | "swapped" | "committed"
    ) {
        return Err(format!(
            "a migration journal states phase `{}`, which Oxide never writes",
            journal.phase
        ));
    }
    let expected = if journal.phase == "committed" {
        receipt_generation
    } else {
        receipt_generation.saturating_add(1)
    };
    if journal.generation != expected || journal.generation == 0 {
        return Err(format!(
            "a migration journal names generation {}, but its phase needs {expected}",
            journal.generation
        ));
    }
    if journal.link != checkout.join("target") {
        return Err(format!(
            "a migration journal names link {}, not the checkout's own target",
            journal.link.display()
        ));
    }
    let mut sorted = journal.members.clone();
    sorted.sort();
    sorted.dedup();
    if journal.members.is_empty() || sorted != journal.members {
        return Err("a migration journal's member set is empty, unsorted or not unique".to_owned());
    }
    for path in [&journal.stage, &journal.backup, &journal.link_text] {
        if !path.is_absolute() {
            return Err(format!(
                "a migration journal names a relative path {}",
                path.display()
            ));
        }
    }
    if journal.link_dev == 0
        || journal.link_ino == 0
        || journal.parent_dev == 0
        || journal.parent_ino == 0
    {
        return Err("a migration journal names no usable link or parent identity".to_owned());
    }
    if let Some(pin) = &journal.marker_pin {
        if pin.dev == 0
            || pin.ino == 0
            || pin.sha256.len() != 64
            || !pin.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("a migration journal pins no usable old marker".to_owned());
        }
    }
    if journal.prior_receipt_sha.len() != 64
        || !journal
            .prior_receipt_sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("a migration journal pins no 64-hex receipt digest".to_owned());
    }
    match journal.root.nonce.as_deref() {
        Some(nonce) if nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) => {}
        _ => {
            return Err(
                "a migration journal needs one 128-bit root nonce for the root it keeps".to_owned(),
            )
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrepared {
    #[allow(dead_code)]
    generation: u64,
    to: RawSelected,
    #[allow(dead_code)]
    link: PathBuf,
}

fn parse_receipt(
    bytes: &[u8],
    checkout_id: &str,
) -> std::result::Result<OxideReceiptReference, String> {
    let raw: RawReceipt = serde_json::from_slice(bytes)
        .map_err(|error| format!("a receipt is not the field set Oxide writes: {error}"))?;
    if !matches!(
        raw.version.as_str(),
        OXIDE_RECEIPT_V1 | OXIDE_RECEIPT_V2 | OXIDE_RECEIPT_V3
    ) {
        return Err(format!(
            "a receipt states `{}`, which is not a version this build reads",
            raw.version
        ));
    }
    if raw.checkout_id != checkout_id {
        return Err(format!(
            "the receipt for {checkout_id} names checkout id {}",
            raw.checkout_id
        ));
    }
    if !matches!(
        raw.state.as_str(),
        "committed" | "prepared" | "detached" | "pruned" | "pruning"
    ) {
        return Err(format!(
            "a receipt states `{}`, which is not a state Oxide writes",
            raw.state
        ));
    }
    // Each version states its own shape. A document whose shape belongs to another
    // version is contradictory rather than merely old, so it is refused.
    match raw.version.as_str() {
        OXIDE_RECEIPT_V1 => {
            if raw.layout.is_some()
                || raw.transition.is_some()
                || !raw.links.is_empty()
                || raw.reserved.is_some()
                || raw.pruned.is_some()
                || matches!(raw.state.as_str(), "pruned" | "pruning")
            {
                return Err(
                    "a version-1 receipt carries only its whole-target selection".to_owned(),
                );
            }
            if raw.selected.is_none() && raw.prepared.is_none() {
                return Err(
                    "a version-1 receipt names neither a selection nor a prepared root".to_owned(),
                );
            }
        }
        OXIDE_RECEIPT_V2 => {
            if raw.layout.is_some() || raw.transition.is_some() {
                return Err(
                    "a version-2 receipt carries no successor layout or migration".to_owned(),
                );
            }
            if raw.selected.is_some() || raw.prepared.is_some() {
                return Err(
                    "a version-2 receipt carries placements, not top-level references".to_owned(),
                );
            }
        }
        _ => {
            if raw.selected.is_some() || raw.prepared.is_some() {
                return Err(
                    "a version-3 receipt carries placements, not top-level references".to_owned(),
                );
            }
            match raw.layout.as_deref() {
                Some("profile") if raw.transition.is_none() => {}
                // A migration journal is published in the successor format before
                // its commit; its placement is still the legacy whole target.
                Some("whole_target") if raw.transition.is_some() => {}
                Some(other) => {
                    return Err(format!(
                        "a version-3 receipt declares layout `{other}`, not profile or whole_target"
                    ))
                }
                None => return Err("a version-3 receipt must declare its layout".to_owned()),
            }
            if raw
                .links
                .iter()
                .any(|placement| placement.reserved.is_some())
            {
                return Err("a version-3 reservation is root-level, not per placement".to_owned());
            }
        }
    }
    // The placements must be the ones this receipt's layout holds. A version-1
    // receipt names its one reference at the top level instead of a placement, so
    // it is the only whole-target shape without one.
    let whole_target = raw.version == OXIDE_RECEIPT_V1
        || raw.version == OXIDE_RECEIPT_V2
        || raw.layout.as_deref() == Some("whole_target");
    if whole_target {
        if raw.links.len() > 1
            || raw
                .links
                .iter()
                .any(|placement| placement.member != "target")
        {
            return Err("a whole-target receipt holds exactly one `target` placement".to_owned());
        }
        if raw.version != OXIDE_RECEIPT_V1 && raw.links.len() != 1 {
            return Err("a whole-target receipt holds exactly one `target` placement".to_owned());
        }
    } else {
        if raw.links.is_empty() {
            return Err("a profile receipt holds at least one member placement".to_owned());
        }
        let members: Vec<&String> = raw
            .links
            .iter()
            .map(|placement| &placement.member)
            .collect();
        let mut sorted = members.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != members {
            return Err(
                "a profile receipt's placements are not unique and sorted by member".to_owned(),
            );
        }
    }
    if raw.roots.len() > 8 {
        return Err("a receipt holds more roots than Oxide keeps".to_owned());
    }
    if let Some(journal) = &raw.transition {
        validate_migration(journal, &raw.checkout, raw.generation)?;
    }
    // Every path a receipt names is absolute, as Oxide's own receipt validation
    // requires: a selection, a history entry, a prepared destination, a reservation and
    // a migration journal root all name real roots, and a relative one names nothing
    // this reader could ever match.
    for placement in &raw.links {
        if placement
            .selected
            .as_ref()
            .is_some_and(|selected| !is_safe_absolute(&selected.root))
            || placement.prepared.as_ref().is_some_and(|prepared| {
                !is_safe_absolute(&prepared.to.root) || !prepared.link.is_absolute()
            })
            || placement
                .reserved
                .as_ref()
                .is_some_and(|reserved| !is_safe_absolute(&reserved.root))
        {
            return Err(
                "a receipt names a root path that is relative or has a `.` or `..` component"
                    .to_owned(),
            );
        }
    }
    for selected in raw
        .selected
        .iter()
        .chain(raw.prepared.as_ref().map(|prepared| &prepared.to))
        .chain(raw.roots.iter())
        .chain(raw.transition.as_ref().map(|journal| &journal.root))
    {
        if !is_safe_absolute(&selected.root) {
            return Err(
                "a receipt names a root path that is relative or has a `.` or `..` component"
                    .to_owned(),
            );
        }
    }
    if raw
        .reserved
        .as_ref()
        .is_some_and(|reserved| !is_safe_absolute(&reserved.root))
    {
        return Err("a receipt names a relative root".to_owned());
    }
    // Every root a receipt names states its kind, and only Oxide's two kinds are
    // understood. A selection, a history entry, a reservation or a journal root
    // of an unknown kind is a divergence rather than a root of no kind.
    for placement in &raw.links {
        if let Some(selected) = &placement.selected {
            check_root_kind("a receipt placement", &selected.kind)?;
        }
        if let Some(prepared) = &placement.prepared {
            check_root_kind("a prepared placement", &prepared.to.kind)?;
        }
        if let Some(reserved) = &placement.reserved {
            check_root_kind("a receipt reservation", &reserved.kind)?;
        }
    }
    if let Some(selected) = &raw.selected {
        check_root_kind("a receipt reference", &selected.kind)?;
    }
    if let Some(prepared) = &raw.prepared {
        check_root_kind("a prepared reference", &prepared.to.kind)?;
    }
    for history in &raw.roots {
        check_root_kind("a receipt history entry", &history.kind)?;
    }
    if let Some(reserved) = &raw.reserved {
        check_root_kind("a receipt reservation", &reserved.kind)?;
    }
    if let Some(journal) = &raw.transition {
        check_root_kind("a migration journal", &journal.root.kind)?;
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut current: Vec<PathBuf> = Vec::new();
    let note =
        |root: &PathBuf, is_current: bool, roots: &mut Vec<PathBuf>, current: &mut Vec<PathBuf>| {
            if !roots.iter().any(|seen| seen == root) {
                roots.push(root.clone());
            }
            if is_current && !current.iter().any(|seen| seen == root) {
                current.push(root.clone());
            }
        };
    for placement in &raw.links {
        if let Some(selected) = &placement.selected {
            note(&selected.root, true, &mut roots, &mut current);
        }
        if let Some(prepared) = &placement.prepared {
            note(&prepared.to.root, true, &mut roots, &mut current);
        }
        // A version-1 receipt keeps its reservation inside its placement.
        if let Some(reserved) = &placement.reserved {
            note(&reserved.root, true, &mut roots, &mut current);
        }
    }
    if let Some(reserved) = &raw.reserved {
        note(&reserved.root, true, &mut roots, &mut current);
    }
    // A version-1 whole-target receipt states its one reference at the top
    // level instead of inside a placement.
    if let Some(selected) = &raw.selected {
        note(&selected.root, true, &mut roots, &mut current);
    }
    if let Some(prepared) = &raw.prepared {
        note(&prepared.to.root, true, &mut roots, &mut current);
    }
    // A prepared migration's committed successor is the current root; its
    // history is not.
    for history in &raw.roots {
        note(&history.root, false, &mut roots, &mut current);
    }
    if let Some(journal) = &raw.transition {
        // A journal is present from its publication until its cleanup, and the
        // root it names is the one this checkout keeps: the checkout is mid-move,
        // so that root is current until the journal settles.
        note(&journal.root.root, true, &mut roots, &mut current);
    }
    Ok(OxideReceiptReference {
        checkout_id: checkout_id.to_owned(),
        checkout: raw.checkout,
        roots,
        current,
    })
}

/// Read one root's own Oxide marker, when it carries one.
///
/// The marker is read through the opened root descriptor, bounded, and never
/// followed through a link. An `oxide.outputs.root/1` marker is reported as
/// unknown: its field set predates the nonce and the checkout volume, and
/// reading it as version 2 would invent facts.
pub fn read_oxide_marker(
    dirs: &dyn DirOps,
    root: &DirHandle,
) -> std::result::Result<Option<OxideMarker>, String> {
    Ok(read_oxide_marker_full(dirs, root)?.map(|full| OxideMarker {
        checkout: full.checkout,
        checkout_id: full.checkout_id,
        checkout_volume: full.checkout_volume,
    }))
}

/// One root's Oxide marker, with everything the adapter binds: the exact
/// bytes' SHA-256, the marker's root nonce and its own volume UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideMarkerFull {
    /// The checkout the marker names.
    pub checkout: PathBuf,
    /// The checkout's id.
    pub checkout_id: String,
    /// The checkout's APFS volume UUID, when the marker records one.
    pub checkout_volume: Option<VolumeUuid>,
    /// The root's immutable nonce, when the marker records one.
    pub nonce: Option<String>,
    /// The root volume's UUID, when the marker records one.
    pub volume_uuid: Option<VolumeUuid>,
    /// The SHA-256 of the exact marker bytes this reading verified.
    pub digest: String,
}

/// [`read_oxide_marker`], keeping the fields the adapter binds.
///
/// The read and the strictness are the same function's: one bounded,
/// descriptor-relative, byte-exact read whose digest covers exactly the bytes
/// the version check and the field check accepted. A `/1` marker is refused
/// here too: it is not the covered class, and its missing nonce must never be
/// read as a match.
pub fn read_oxide_marker_full(
    dirs: &dyn DirOps,
    root: &DirHandle,
) -> std::result::Result<Option<OxideMarkerFull>, String> {
    #[cfg(not(test))]
    let facts = match dirs.child_facts(root, OXIDE_MARKER_FILE) {
        Ok(Some(facts)) => facts,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("the marker cannot be read: {error}")),
    };
    #[cfg(test)]
    let facts = match dirs.child_facts(root, OXIDE_MARKER_FILE) {
        Ok(Some(facts)) => facts,
        Ok(None) => {
            #[cfg(test)]
            crate::tape::child_stat(
                "reference::read_oxide_marker",
                (root.identity().device.get(), root.identity().inode),
                OXIDE_MARKER_FILE,
                None,
                None,
            );
            return Ok(None);
        }
        Err(error) => {
            #[cfg(test)]
            crate::tape::child_stat(
                "reference::read_oxide_marker",
                (root.identity().device.get(), root.identity().inode),
                OXIDE_MARKER_FILE,
                None,
                Some(error.kind().as_str()),
            );
            return Err(format!("the marker cannot be read: {error}"));
        }
    };
    if facts.kind != crate::os::EntryKind::File {
        return Err("the Oxide marker is not a regular file".to_owned());
    }
    #[cfg(not(test))]
    let bytes =
        match dirs.read_child_file(root, OXIDE_MARKER_FILE, MARKER_CAP_BYTES) {
            Ok(Some(bytes)) => bytes,
            // The marker was observed a moment ago; a read that finds nothing is a
            // change under the observation, never "this root has no Oxide marker".
            Ok(None) => return Err(
                "the Oxide marker was observed and is no longer there, so the observation changed"
                    .to_owned(),
            ),
            Err(error) => return Err(format!("the marker cannot be read: {error}")),
        };
    #[cfg(test)]
    let bytes = match dirs.read_child_file(root, OXIDE_MARKER_FILE, MARKER_CAP_BYTES) {
        Ok(Some(bytes)) => {
            #[cfg(test)]
            crate::tape::read_present_component(
                "reference::read_oxide_marker",
                (root.identity().device.get(), root.identity().inode),
                OXIDE_MARKER_FILE,
                &bytes,
                Some(MARKER_CAP_BYTES as u64),
            );
            bytes
        }
        // The marker was observed a moment ago; a read that finds nothing is a
        // change under the observation, never "this root has no Oxide marker".
        Ok(None) => {
            crate::tape::read_child_refused(
                "reference::read_oxide_marker",
                (root.identity().device.get(), root.identity().inode),
                OXIDE_MARKER_FILE,
                "disappeared",
            );
            return Err(
                "the Oxide marker was observed and is no longer there, so the observation changed"
                    .to_owned(),
            );
        }
        Err(error) => {
            crate::tape::read_child_refused(
                "reference::read_oxide_marker",
                (root.identity().device.get(), root.identity().inode),
                OXIDE_MARKER_FILE,
                error.kind().as_str(),
            );
            return Err(format!("the marker cannot be read: {error}"));
        }
    };
    // The bytes are evidence only if the same entry is still the one that was
    // observed before the read.
    match dirs.child_facts(root, OXIDE_MARKER_FILE) {
        Ok(Some(after)) if after == facts => {}
        Ok(_) => {
            return Err(
                "the Oxide marker changed while it was read, so the observation is not stable"
                    .to_owned(),
            )
        }
        Err(error) => return Err(format!("the marker cannot be read: {error}")),
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the marker is not one JSON document: {error}"))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "the marker states no version".to_owned())?;
    if version == OXIDE_MARKER_V1 {
        return Err(format!(
            "`{version}` predates the field set this reader needs; it is not read"
        ));
    }
    if version != OXIDE_MARKER_V2 {
        return Err(format!("`{version}` is not a version this build reads"));
    }
    let raw: RawMarker = serde_json::from_value(value)
        .map_err(|error| format!("the marker is not the field set Oxide writes: {error}"))?;
    // The marker states its root kind too, and only Oxide's two kinds are
    // understood: a divergent marker is unreadable ownership, so the reference set
    // it belongs to is incomplete and its root is kept.
    check_root_kind("the root's Oxide marker", &raw.kind)?;
    let checkout_volume = match raw.checkout_volume_uuid.as_deref() {
        Some(raw) => Some(
            VolumeUuid::parse(raw)
                .map_err(|error| format!("the marker's checkout volume is not a UUID: {error}"))?,
        ),
        None => None,
    };
    let volume_uuid = match raw.volume_uuid.as_deref() {
        Some(raw) => Some(
            VolumeUuid::parse(raw)
                .map_err(|error| format!("the marker's volume is not a UUID: {error}"))?,
        ),
        None => None,
    };
    Ok(Some(OxideMarkerFull {
        checkout: raw.checkout,
        checkout_id: raw.checkout_id,
        checkout_volume,
        nonce: raw.nonce,
        volume_uuid,
        digest: crate::digest::sha256_hex(&bytes),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMarker {
    #[allow(dead_code)]
    version: String,
    checkout: PathBuf,
    checkout_id: String,
    // Oxide writes all of these; a marker that omits one is a document this build
    // does not understand, never a marker with a defaulted fact.
    #[allow(dead_code)]
    label: String,
    #[allow(dead_code)]
    recipe: String,
    kind: String,
    #[serde(default)]
    #[allow(dead_code)]
    volume_uuid: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    nonce: Option<String>,
    #[serde(default)]
    checkout_volume_uuid: Option<String>,
    #[allow(dead_code)]
    generation: u64,
    #[allow(dead_code)]
    created_ms: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A schema-3 document with the same unchanged prune-intent field set.
    fn routed_registry() -> serde_json::Value {
        serde_json::json!({
            "version": OXIDE_REGISTRY_V3, "schema": 3, "revision": 1,
            "complete": true, "created_by": "oxide", "rows": [],
            "intents": [{
                "version": OXIDE_INTENT_V1, "id": "1".repeat(32),
                "identity": {
                    "checkout_id": "c".repeat(64), "checkout": "/c", "receipt": "/r",
                    "member": "target", "root": "/o", "root_kind": "external",
                    "generation": 1, "dev": 7, "ino": 42, "marker_digest": "a".repeat(64)
                },
                "phase": "intent", "quarantine": "/q", "preview_sha256": "0".repeat(64),
                "created_ms": 1, "updated_ms": 2
            }],
            "deletion_routes": [{"intent_id": "1".repeat(32), "deleter": "barge", "request_sha256": "b".repeat(64)}]
        })
    }

    #[test]
    fn schema_three_routes_are_explicit_for_every_retained_phase() {
        for phase in [
            "intent",
            "renamed",
            "deleting",
            "marker_last",
            "deleted",
            "resolved",
        ] {
            for route in [
                serde_json::json!({"intent_id": "1".repeat(32), "deleter": "local"}),
                serde_json::json!({"intent_id": "1".repeat(32), "deleter": "barge", "request_sha256": "b".repeat(64)}),
            ] {
                let mut document = routed_registry();
                document["intents"][0]["phase"] = serde_json::json!(phase);
                document["deletion_routes"][0] = route.clone();
                let reading =
                    parse_registry(&serde_json::to_vec(&document).expect("fixture encodes"));
                let Ok(OxideRegistry::Read { intents, .. }) = reading else {
                    panic!("valid explicit route reads: {reading:?}")
                };
                assert_eq!(intents[0].phase, phase);
                let expected = if route["deleter"] == "local" {
                    DeletionRoute::Local
                } else {
                    DeletionRoute::Barge {
                        request_sha256: "b".repeat(64),
                    }
                };
                assert_eq!(intents[0].deletion_route, Some(expected));
            }
        }
    }

    #[test]
    fn malformed_or_incomplete_routes_never_read_as_absence() {
        let valid = routed_registry();
        let route = valid["deletion_routes"][0].clone();
        let mut cases = vec![
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!([route.clone(), route.clone()]),
        ];
        for (field, value) in [
            ("intent_id", serde_json::json!("2".repeat(32))),
            ("intent_id", serde_json::json!("bad")),
            ("deleter", serde_json::json!("unknown")),
            ("request_sha256", serde_json::json!("A".repeat(64))),
            ("request_sha256", serde_json::json!("g".repeat(64))),
            ("request_sha256", serde_json::json!("b".repeat(63))),
            ("invented", serde_json::json!(true)),
        ] {
            let mut changed = route.clone();
            changed[field] = value;
            cases.push(serde_json::json!([changed]));
        }
        cases.push(serde_json::json!([{"intent_id":"1".repeat(32),"deleter":"barge"}]));
        cases.push(serde_json::json!([{"intent_id":"1".repeat(32),"deleter":"local","request_sha256":"b".repeat(64)}]));
        for routes in cases {
            let mut document = valid.clone();
            document["deletion_routes"] = routes;
            assert!(
                parse_registry(&serde_json::to_vec(&document).expect("fixture encodes")).is_err()
            );
        }
        let mut missing = valid.clone();
        missing
            .as_object_mut()
            .expect("object")
            .remove("deletion_routes");
        assert!(parse_registry(&serde_json::to_vec(&missing).expect("fixture encodes")).is_err());
        for schema in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(2),
            serde_json::json!(4),
        ] {
            let mut document = valid.clone();
            document["schema"] = schema;
            assert!(
                parse_registry(&serde_json::to_vec(&document).expect("fixture encodes")).is_err()
            );
        }
        let mut duplicate_intent = valid;
        let intent = duplicate_intent["intents"][0].clone();
        duplicate_intent["intents"] = serde_json::json!([intent.clone(), intent]);
        duplicate_intent["deletion_routes"] =
            serde_json::json!([route.clone(), {"intent_id":"2".repeat(32),"deleter":"local"}]);
        assert!(
            parse_registry(&serde_json::to_vec(&duplicate_intent).expect("fixture encodes"))
                .is_err()
        );
    }

    #[test]
    fn routes_share_the_intent_cap_and_do_not_change_historical_formats() {
        let mut document = routed_registry();
        let mut intents = Vec::new();
        let mut routes = Vec::new();
        for index in 0..257 {
            let id = format!("{index:032x}");
            let mut intent = document["intents"][0].clone();
            intent["id"] = serde_json::json!(id);
            intents.push(intent);
            routes.push(serde_json::json!({"intent_id":id,"deleter":"local"}));
            if index >= 255 {
                document["intents"] = serde_json::json!(intents);
                document["deletion_routes"] = serde_json::json!(routes);
                assert_eq!(
                    parse_registry(&serde_json::to_vec(&document).expect("fixture encodes"))
                        .is_ok(),
                    index == 255
                );
            }
        }
        for (version, schema) in [(OXIDE_REGISTRY_V1, 1), (OXIDE_REGISTRY_V2, 2)] {
            let mut historical = routed_registry();
            historical["version"] = serde_json::json!(version);
            historical["schema"] = serde_json::json!(schema);
            assert!(
                parse_registry(&serde_json::to_vec(&historical).expect("fixture encodes")).is_err()
            );
            historical
                .as_object_mut()
                .expect("object")
                .remove("deletion_routes");
            let Ok(OxideRegistry::Read { intents, .. }) =
                parse_registry(&serde_json::to_vec(&historical).expect("fixture encodes"))
            else {
                panic!("historical document reads")
            };
            assert_eq!(intents[0].deletion_route, None);
        }
    }

    /// A coordinated-registry document exactly as Oxide writes it after
    /// adopting single authority (Oxide at 8346df5e): the exact version/schema
    /// pair, no `deletion_routes`, and a present `settled` array whose entries
    /// are exactly Oxide's `Settlement` field set in its serialization order.
    fn coordinated_registry() -> serde_json::Value {
        serde_json::json!({
            "version": OXIDE_REGISTRY_V4, "schema": 4, "revision": 2,
            "complete": true, "created_by": "oxide", "rows": [],
            "settled": [],
        })
    }

    /// One settlement entry, exactly Oxide's serialized `Settlement` field set:
    /// the caller, a 32-hex key, a 64-hex checkout id, an absolute root, a
    /// present 32-hex nonce, the `deleted` outcome, a positive terminal time
    /// and a 64-hex request digest.
    fn settlement_entry(root: &str) -> serde_json::Value {
        serde_json::json!({
            "caller": "oxide.outputs",
            "key": "a".repeat(32),
            "checkout_id": "c".repeat(64),
            "root": root,
            "nonce": "b".repeat(32),
            "outcome": "deleted",
            "terminal_ms": 1,
            "request_sha256": "d".repeat(64),
        })
    }

    #[test]
    fn a_coordinated_registry_requires_its_present_settlement_set() {
        // The adopted empty set is Oxide's own upgrade shape: present, empty,
        // and never inferred.
        let read = parse_registry(&serde_json::to_vec(&coordinated_registry()).expect("encodes"));
        assert!(matches!(
            read,
            Ok(OxideRegistry::Read { complete: true, .. })
        ));

        // A settlement entry with every field exactly right reads.
        let mut with_entry = coordinated_registry();
        with_entry["settled"] = serde_json::json!([settlement_entry("/Volumes/Builds/gone")]);
        assert!(matches!(
            parse_registry(&serde_json::to_vec(&with_entry).expect("encodes")),
            Ok(OxideRegistry::Read { .. })
        ));

        // Absent, null, and not-an-array are all refusals: neither ever reads
        // as an empty set or as absence of the member.
        let mut absent = coordinated_registry();
        absent.as_object_mut().expect("object").remove("settled");
        assert!(parse_registry(&serde_json::to_vec(&absent).expect("encodes")).is_err());
        let mut null = coordinated_registry();
        null["settled"] = serde_json::Value::Null;
        assert!(parse_registry(&serde_json::to_vec(&null).expect("encodes")).is_err());
        let mut string = coordinated_registry();
        string["settled"] = serde_json::json!("gone");
        assert!(parse_registry(&serde_json::to_vec(&string).expect("encodes")).is_err());
    }

    #[test]
    fn a_settlement_matrix_refuses_every_unusable_entry() {
        let valid = settlement_entry("/Volumes/Builds/gone");
        let mut cases = Vec::new();
        for (field, value) in [
            ("caller", serde_json::json!("oxide.other")),
            ("caller", serde_json::json!("")),
            ("key", serde_json::json!("A".repeat(32))),
            ("key", serde_json::json!("a".repeat(31))),
            ("key", serde_json::json!("g".repeat(32))),
            ("key", serde_json::json!("")),
            ("checkout_id", serde_json::json!("c".repeat(63))),
            ("checkout_id", serde_json::json!("C".repeat(64))),
            ("root", serde_json::json!("gone")),
            ("root", serde_json::json!("/Volumes/Builds/../Builds/gone")),
            ("nonce", serde_json::Value::Null),
            ("nonce", serde_json::json!("b".repeat(31))),
            ("nonce", serde_json::json!("B".repeat(32))),
            ("outcome", serde_json::json!("removed")),
            ("outcome", serde_json::json!("")),
            ("terminal_ms", serde_json::json!(0)),
            ("request_sha256", serde_json::json!("d".repeat(63))),
            ("request_sha256", serde_json::json!("D".repeat(64))),
            ("invented", serde_json::json!(true)),
        ] {
            let mut changed = valid.clone();
            changed[field] = value;
            cases.push(changed);
        }
        // A missing nonce member is not a present one: it reads as absent and
        // an absent nonce is refused.
        let mut without_nonce = valid.clone();
        without_nonce
            .as_object_mut()
            .expect("object")
            .remove("nonce");
        cases.push(without_nonce);
        // The same settlement twice is a repeat.
        cases.push(serde_json::json!([valid.clone(), valid.clone()]));
        // The same key under the one caller twice is a repeat too.
        let mut other_root = valid.clone();
        other_root["root"] = serde_json::json!("/Volumes/Builds/also-gone");
        cases.push(serde_json::json!([valid, other_root]));
        for settled in cases {
            let mut document = coordinated_registry();
            document["settled"] = serde_json::json!([settled]);
            assert!(
                parse_registry(&serde_json::to_vec(&document).expect("encodes")).is_err(),
                "the settlement must be refused: {document}"
            );
        }

        // The retained set shares Oxide's 256-entry cap: 256 entries read,
        // 257 refuse.
        for count in [256, 257] {
            let entries: Vec<serde_json::Value> = (0..count)
                .map(|index| {
                    let mut entry = settlement_entry("/Volumes/Builds/gone");
                    entry["key"] = serde_json::json!(format!("{index:032x}"));
                    entry
                })
                .collect();
            let mut document = coordinated_registry();
            document["settled"] = serde_json::Value::Array(entries);
            assert_eq!(
                parse_registry(&serde_json::to_vec(&document).expect("encodes")).is_ok(),
                count == 256,
                "the settlement cap binds at {count} entries"
            );
        }
    }

    #[test]
    fn settlement_facts_belong_only_to_the_coordinated_format() {
        // Every predecessor refuses the member: a settled array must never
        // retroactively attach to a format that never wrote one.
        for (version, schema) in [
            (OXIDE_REGISTRY_V1, serde_json::json!(1)),
            (OXIDE_REGISTRY_V2, serde_json::json!(2)),
            (OXIDE_REGISTRY_V3, serde_json::json!(3)),
        ] {
            let mut predecessor = coordinated_registry();
            predecessor["version"] = serde_json::json!(version);
            predecessor["schema"] = schema;
            assert!(
                parse_registry(&serde_json::to_vec(&predecessor).expect("encodes")).is_err(),
                "{version} must refuse settled"
            );
        }

        // The coordinated format refuses the predecessor's routes, with or
        // without its own settlement set.
        for settled in [serde_json::Value::Null, serde_json::json!([])] {
            let mut routed = coordinated_registry();
            routed["deletion_routes"] =
                serde_json::json!([{"intent_id": "1".repeat(32), "deleter": "local"}]);
            if settled.is_null() {
                routed.as_object_mut().expect("object").remove("settled");
            }
            assert!(parse_registry(&serde_json::to_vec(&routed).expect("encodes")).is_err());
        }

        // The version/schema pair is exact: no other numeric format states the
        // coordinated version, and the schema is always written.
        for schema in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
            serde_json::json!(5),
        ] {
            let mut document = coordinated_registry();
            if schema.is_null() {
                document.as_object_mut().expect("object").remove("schema");
            } else {
                document["schema"] = schema.clone();
            }
            assert!(
                parse_registry(&serde_json::to_vec(&document).expect("encodes")).is_err(),
                "schema {schema} must refuse the coordinated version"
            );
        }
    }

    #[test]
    fn a_settlement_is_a_terminal_fact_that_never_names_a_root() {
        // A settlement whose root path is the only record of a root: the
        // registry reads with no rows and no intents. The terminal fact alone
        // contributes nothing a reference depends on.
        let mut alone = coordinated_registry();
        alone["settled"] = serde_json::json!([settlement_entry("/Volumes/Builds/gone")]);
        let Ok(OxideRegistry::Read { rows, intents, .. }) =
            parse_registry(&serde_json::to_vec(&alone).expect("encodes"))
        else {
            panic!("the terminal fact alone reads")
        };
        assert!(rows.is_empty() && intents.is_empty());

        // A settled history row and a settlement naming the same path
        // coexist: the row is present and not live, and the settlement adds
        // nothing. A settlement never turns a row's path into a reference.
        let mut both = coordinated_registry();
        both["rows"] = serde_json::json!([
            {
                "checkout_id": "c".repeat(64),
                "checkout": "/Users/fixture/Dev/example",
                "receipt": "/pool/receipt.json",
                "member": "target",
                "root": "/Volumes/Builds/gone",
                "root_kind": "external",
                "recipe": "target-debug-v1",
                "generation": 1,
                "selected": false,
                "pending": false,
            }
        ]);
        both["settled"] = serde_json::json!([settlement_entry("/Volumes/Builds/gone")]);
        let Ok(OxideRegistry::Read { rows, intents, .. }) =
            parse_registry(&serde_json::to_vec(&both).expect("encodes"))
        else {
            panic!("the row and the terminal fact coexist")
        };
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].live(), "a settled row stays settled: {rows:?}");
        assert!(intents.is_empty());
    }

    #[test]
    fn a_registry_row_that_is_not_settled_names_its_root() {
        let row = OxideRow {
            checkout_id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                .to_owned(),
            checkout: PathBuf::from("/Users/fixture/Dev/example"),
            checkout_volume: None,
            root: PathBuf::from("/Volumes/Builds/oxide/outputs/root"),
            nonce: None,
            selected: false,
            pending: false,
            pruning: false,
            reservation: false,
            cancelling: true,
        };
        assert!(row.live(), "Oxide's own Row::live includes pending");
        assert_eq!(row.flags(), "cancelling");
    }

    #[test]
    fn policy_without_an_age_never_releases() {
        assert!(!policy_permits_release(&PrunePolicy::None));
        assert!(policy_permits_release(&PrunePolicy::OldestIdleAfter {
            age_seconds: 60
        }));
    }

    #[test]
    fn both_registry_majors_read_and_an_unknown_one_is_unknown() {
        let v2 = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":3,"complete":true,"created_by":"oxide","rows":[{"checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","checkout":"/Users/fixture/Dev/example","receipt":"/pool/receipt.json","member":"target","root":"/Volumes/Builds/oxide/outputs/root","root_kind":"external","recipe":"target-debug-v1","nonce":"00112233445566778899aabbccddeeff","generation":1,"root_dev":7,"root_ino":42,"marker_digest":"aa","selected":false,"pending":false}]}"#;
        let OxideRegistry::Read { rows, complete, .. } = parse_registry(v2).expect("v2 reads")
        else {
            panic!("a registry that reads is a Read");
        };
        assert!(complete);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].root,
            PathBuf::from("/Volumes/Builds/oxide/outputs/root")
        );
        assert!(!rows[0].live(), "a settled row names no root as wanted");

        let v1 = br#"{"version":"oxide.outputs.registry/1","revision":1,"complete":true,"created_by":"oxide","rows":[]}"#;
        assert!(matches!(
            parse_registry(v1),
            Ok(OxideRegistry::Read { rows, .. }) if rows.is_empty()
        ));
        let unknown = br#"{"version":"oxide.outputs.registry/9","revision":1,"complete":true,"created_by":"oxide","rows":[]}"#;
        assert!(parse_registry(unknown).is_err());
        let divergent = br#"{"version":"oxide.outputs.registry/2","revision":1,"complete":true,"created_by":"oxide","rows":[],"unexpected":true}"#;
        assert!(parse_registry(divergent).is_err());
    }

    #[test]
    fn an_unfinished_intent_names_its_root() {
        let bytes = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":2,"complete":true,"created_by":"oxide","rows":[],"intents":[{"version":"oxide.outputs.prune-intent/1","id":"11111111111111111111111111111111","identity":{"checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","checkout":"/Users/fixture/Dev/example","receipt":"/pool/outputs/c1.json","member":"target","root":"/Volumes/Builds/oxide/outputs/root","root_kind":"external","generation":1,"dev":7,"ino":42,"marker_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"phase":"renamed","quarantine":"/pool/outputs/.prune-11111111111111111111111111111111","preview_sha256":"0000000000000000000000000000000000000000000000000000000000000000","created_ms":1,"updated_ms":2}]}"#;
        let OxideRegistry::Read { intents, .. } =
            parse_registry(bytes).expect("the registry reads")
        else {
            panic!("a registry that reads is a Read");
        };
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].phase, "renamed");
        assert_eq!(
            intents[0].root,
            PathBuf::from("/Volumes/Builds/oxide/outputs/root")
        );
    }

    #[test]
    fn a_registry_row_with_an_unusable_identity_is_divergent() {
        // Oxide's own `Row::validate`: a lowercase 64-hex checkout id, bounded member
        // and recipe text, three absolute paths, and never a pruning row that is also
        // selected or pending. Each document below breaks exactly one rule.
        let id = "c".repeat(64);
        let row = |overrides: &str| {
            format!(
                r#"{{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[{{"checkout_id":"{id}","checkout":"/c","receipt":"/r","member":"target","root":"/o","root_kind":"external","recipe":"target-debug-v1","generation":1,"selected":false,"pending":false{overrides}}}]}}"#
            )
        };
        let readable = serde_json::to_vec(&serde_json::json!({
            "version": OXIDE_REGISTRY_V2,
            "schema": 2,
            "revision": 1,
            "complete": true,
            "created_by": "oxide",
            "rows": [{
                "checkout_id": id,
                "checkout": "/c",
                "receipt": "/r",
                "member": "target",
                "root": "/o",
                "root_kind": "external",
                "recipe": "target-debug-v1",
                "generation": 1,
                "selected": false,
                "pending": false,
            }],
        }))
        .expect("the fixture encodes");
        assert!(parse_registry(&readable).is_ok(), "the valid row reads");

        let divergent = [
            // A relative root.
            row("").replace(r#""root":"/o""#, r#""root":"relative""#),
            // A relative checkout.
            row("").replace(r#""checkout":"/c""#, r#""checkout":"example""#),
            // A relative receipt.
            row("").replace(r#""receipt":"/r""#, r#""receipt":"receipt.json""#),
            // An empty member.
            row("").replace(r#""member":"target""#, r#""member":"""#),
            // An empty recipe.
            row("").replace(r#""recipe":"target-debug-v1""#, r#""recipe":"""#),
            // A checkout id that is not a 64-hex digest, and one in uppercase.
            row("").replace(&id, "checkout-1"),
            row("").replace(&id, &"C".repeat(64)),
            // A pruning row that is still selected.
            row(r#","pruning":"intent-1""#).replace(r#""selected":false"#, r#""selected":true"#),
        ];
        for (index, document) in divergent.iter().enumerate() {
            assert!(
                parse_registry(document.as_bytes()).is_err(),
                "divergent registry row {index} was accepted: {document}"
            );
        }
    }

    #[test]
    fn a_receipt_that_names_a_relative_root_is_divergent() {
        // Every root a receipt names is absolute: a relative one names nothing this
        // reader could match, so the receipt is divergent rather than a reference to
        // no root.
        let receipt = br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[{"member":"target"}],"roots":[{"kind":"external","root":"relative","generation":1}],"updated_ms":9}"#;
        assert!(parse_receipt(receipt, &"c".repeat(64)).is_err());
        let reserved = br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","layout":"profile","links":[{"member":"target"}],"reserved":{"id":"r1","nonce":"00112233445566778899aabbccddeeff","kind":"external","root":"future","generation":2,"created_ms":1},"updated_ms":9}"#;
        assert!(parse_receipt(reserved, &"c".repeat(64)).is_err());
    }

    #[test]
    fn a_root_path_with_a_parent_component_is_divergent() {
        // Oxide's `safe_absolute` refuses `.` and `..` components, and a path with one
        // can never be the component chain of a root Barge walks: accepting it would
        // let the snapshot look complete while the named root looks unreferenced.
        let id = "c".repeat(64);
        let unsafe_root = "/Volumes/Builds/pool/existing/../root";
        let receipt = |site: &str| {
            let selected =
                serde_json::json!({"kind": "external", "root": unsafe_root, "generation": 1});
            let mut document = serde_json::json!({
                "version": OXIDE_RECEIPT_V2,
                "checkout": "/c",
                "checkout_id": id,
                "generation": 1,
                "state": "committed",
                "links": [{"member": "target", "selected": selected}],
                "roots": [],
                "updated_ms": 9
            });
            match site {
                "history" => {
                    document["links"] = serde_json::json!([{"member": "target"}]);
                    document["roots"] = serde_json::json!([
                        {"kind": "external", "root": unsafe_root, "generation": 1}
                    ]);
                }
                "prepared" => {
                    document["links"] = serde_json::json!([{
                        "member": "target",
                        "prepared": {
                            "generation": 2,
                            "to": {"kind": "external", "root": unsafe_root, "generation": 2},
                            "link": "/c/target"
                        }
                    }]);
                }
                "reservation" => {
                    document["links"] = serde_json::json!([{"member": "target"}]);
                    document["reserved"] = serde_json::json!({
                        "id": "r1",
                        "nonce": "00112233445566778899aabbccddeeff",
                        "kind": "external",
                        "root": unsafe_root,
                        "generation": 2,
                        "created_ms": 1
                    });
                }
                _ => {}
            }
            serde_json::to_vec(&document).expect("the fixture encodes")
        };
        for site in ["selection", "history", "prepared", "reservation"] {
            let document = receipt(site);
            assert!(
                parse_receipt(&document, &id).is_err(),
                "a receipt `{site}` with a parent component was accepted"
            );
        }
        // A journal root with one is refused too.
        let journal = serde_json::to_vec(&serde_json::json!({
            "version": OXIDE_RECEIPT_V3,
            "checkout": "/c",
            "checkout_id": id,
            "generation": 3,
            "state": "committed",
            "layout": "whole_target",
            "links": [{"member": "target"}],
            "transition": {
                "generation": 4,
                "phase": "staged",
                "root": {"kind": "external", "root": unsafe_root, "nonce": "00112233445566778899aabbccddeeff", "generation": 3},
                "members": ["debug"],
                "link": "/c/target",
                "link_text": "/r",
                "link_dev": 1,
                "link_ino": 2,
                "parent_dev": 1,
                "parent_ino": 2,
                "stage": "/s",
                "backup": "/b",
                "prior_receipt_sha": "0000000000000000000000000000000000000000000000000000000000000000",
                "registry_revision": 3,
                "created_ms": 1,
                "updated_ms": 2
            },
            "updated_ms": 9
        }))
        .expect("the fixture encodes");
        assert!(parse_receipt(&journal, &id).is_err());

        // The registry row and the intent compare the same way, so their root paths are
        // held to the same rule.
        let row = format!(
            r#"{{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[{{"checkout_id":"{id}","checkout":"/c","receipt":"/r","member":"target","root":"{unsafe_root}","root_kind":"external","recipe":"target-debug-v1","generation":1,"selected":false,"pending":false}}]}}"#
        );
        assert!(parse_registry(row.as_bytes()).is_err());
        let intent = format!(
            r#"{{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[],"intents":[{{"version":"{OXIDE_INTENT_V1}","id":"{}","identity":{{"checkout_id":"{id}","checkout":"/c","receipt":"/r","member":"target","root":"{unsafe_root}","root_kind":"external","generation":1,"dev":7,"ino":42,"marker_digest":"{}"}},"phase":"renamed","quarantine":"/q","preview_sha256":"{}","created_ms":1,"updated_ms":2}}]}}"#,
            "1".repeat(32),
            "a".repeat(64),
            "0".repeat(64),
        );
        assert!(parse_registry(intent.as_bytes()).is_err());
    }

    #[test]
    fn a_registry_must_state_its_rows_even_when_they_are_empty() {
        // An omitted row array is a divergence; an explicit empty array is a complete
        // reading that says there are no rows.
        let without_rows = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide"}"#;
        assert!(
            parse_registry(without_rows).is_err(),
            "a registry without its rows was accepted as complete and empty"
        );
        let empty_rows = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[]}"#;
        assert!(matches!(
            parse_registry(empty_rows),
            Ok(OxideRegistry::Read { rows, .. }) if rows.is_empty()
        ));
    }

    #[test]
    fn a_registry_missing_its_required_facts_is_unknown_never_empty() {
        // Each document below omits a field Oxide always writes, contradicts its
        // own completeness flag, or states an intent phase Oxide never writes. Each
        // is a divergence: it must not be read as an empty, complete registry.
        let divergent: [&[u8]; 6] = [
            // No revision.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"complete":true,"created_by":"oxide","rows":[]}"#,
            // No creator.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"rows":[]}"#,
            // Incomplete without its record.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":false,"created_by":"oxide","rows":[]}"#,
            // Complete with an incomplete record.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"incomplete":{"reason":"x","since_ms":1,"transaction":"t"},"created_by":"oxide","rows":[]}"#,
            // An intent phase Oxide never writes.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[],"intents":[{"version":"oxide.outputs.prune-intent/1","id":"11111111111111111111111111111111","identity":{"checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","checkout":"/c","receipt":"/r","member":"target","root":"/o","root_kind":"external","generation":1,"dev":7,"ino":42,"marker_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"phase":"bogus","quarantine":"/q","preview_sha256":"0000000000000000000000000000000000000000000000000000000000000000","created_ms":1,"updated_ms":2}]}"#,
            // An intent without its root identity.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[],"intents":[{"version":"oxide.outputs.prune-intent/1","id":"11111111111111111111111111111111","identity":{"root":"/o"},"phase":"renamed","quarantine":"/q","preview_sha256":"0000000000000000000000000000000000000000000000000000000000000000","created_ms":1,"updated_ms":2}]}"#,
        ];
        for (index, case) in divergent.iter().enumerate() {
            assert!(
                parse_registry(case).is_err(),
                "divergent registry {index} was accepted as complete: {}",
                String::from_utf8_lossy(case)
            );
        }
        // The valid half: a sticky-incomplete registry with its record reads, and
        // reports itself incomplete to the caller.
        let incomplete = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":false,"incomplete":{"reason":"x","since_ms":1,"transaction":"t"},"created_by":"oxide","rows":[]}"#;
        assert!(matches!(
            parse_registry(incomplete),
            Ok(OxideRegistry::Read {
                complete: false,
                ..
            })
        ));
    }

    #[test]
    fn a_receipt_reads_each_versions_own_selection_site() {
        // A version-2 receipt states its selection inside its placement.
        let v2 = br#"{"version":"oxide.outputs.transition/2","checkout":"/Users/fixture/Dev/example","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":2,"state":"committed","links":[{"member":"target","selected":{"kind":"external","root":"/Volumes/Builds/oxide/outputs/current","generation":2}}],"roots":[{"kind":"external","root":"/Volumes/Builds/oxide/outputs/old","generation":1}],"updated_ms":9}"#;
        let receipt = parse_receipt(
            v2,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .expect("v2 reads");
        assert_eq!(
            receipt.current,
            vec![PathBuf::from("/Volumes/Builds/oxide/outputs/current")]
        );
        assert_eq!(receipt.roots.len(), 2, "history is named but not current");

        // A version-1 receipt states its one whole-target selection at the top.
        let v1 = br#"{"version":"oxide.outputs.transition/1","checkout":"/Users/fixture/Dev/example","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","selected":{"kind":"external","root":"/Volumes/Builds/oxide/outputs/current","generation":1},"updated_ms":9}"#;
        let legacy = parse_receipt(
            v1,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .expect("v1 reads");
        assert_eq!(legacy.current, legacy.roots);

        // A version-3 receipt reserves its next root at the root level.
        let v3 = br#"{"version":"oxide.outputs.transition/3","checkout":"/Users/fixture/Dev/example","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":3,"state":"committed","layout":"profile","links":[{"member":"target"}],"reserved":{"id":"r1","nonce":"0011","kind":"external","root":"/Volumes/Builds/oxide/outputs/reserved","generation":4,"created_ms":1},"updated_ms":9}"#;
        let reserved = parse_receipt(
            v3,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .expect("v3 reads");
        assert_eq!(
            reserved.current,
            vec![PathBuf::from("/Volumes/Builds/oxide/outputs/reserved")]
        );

        let unknown = br#"{"version":"oxide.outputs.transition/9","checkout":"/Users/fixture/Dev/example","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[],"roots":[],"updated_ms":9}"#;
        assert!(parse_receipt(
            unknown,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        )
        .is_err());
        // A receipt whose own id disagrees with its file name is not followed.
        assert!(parse_receipt(v2, "other").is_err());
    }

    #[test]
    fn a_receipt_with_a_missing_field_or_a_wrong_shape_is_divergent() {
        // Every document below omits a field Oxide always writes, states a state
        // this build does not know, or carries another version's shape. Each is a
        // divergence: never a receipt with defaulted facts, and never evidence
        // that a reference set is complete.
        let cases: [&[u8]; 9] = [
            // No `state`.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"links":[],"roots":[],"updated_ms":9}"#,
            // A state Oxide never writes.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"finished","links":[],"roots":[],"updated_ms":9}"#,
            // No `updated_ms`.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[],"roots":[]}"#,
            // A version-3 receipt without its layout.
            br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[],"roots":[],"updated_ms":9}"#,
            // A version-2 receipt with a successor layout.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","layout":"profile","links":[],"roots":[],"updated_ms":9}"#,
            // A version-2 receipt with a top-level reference.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","selected":{"kind":"external","root":"/r","generation":1},"links":[],"roots":[],"updated_ms":9}"#,
            // A version-1 receipt with placements.
            br#"{"version":"oxide.outputs.transition/1","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[{"member":"target"}],"roots":[],"updated_ms":9}"#,
            // A whole-target version-3 receipt without its migration journal.
            br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","layout":"whole_target","links":[],"roots":[],"updated_ms":9}"#,
            // A placement without its member.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[{"selected":{"kind":"external","root":"/r","generation":1}}],"roots":[],"updated_ms":9}"#,
        ];
        for (index, case) in cases.iter().enumerate() {
            assert!(
                parse_receipt(
                    case,
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "case {index} was accepted as a receipt: {}",
                String::from_utf8_lossy(case)
            );
        }
        // A version-3 receipt whose history entry omits its generation is
        // divergent too: Oxide always records it.
        let history = br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":[],"roots":[{"kind":"external","root":"/r"}],"updated_ms":9}"#;
        assert!(parse_receipt(
            history,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        )
        .is_err());
    }

    #[test]
    fn a_migration_journal_is_read_strictly_and_names_the_root_it_keeps() {
        // A whole-target version-3 receipt carries the journal of the move in
        // progress, and the root that journal names is the one the checkout keeps.
        let good = br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":3,"state":"committed","layout":"whole_target","links":[{"member":"target"}],"transition":{"generation":4,"phase":"staged","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["debug","release"],"link":"/c/target","link_text":"/r","link_dev":1,"link_ino":2,"parent_dev":1,"parent_ino":2,"stage":"/c/.target.oxide-new-4","backup":"/c/.target.oxide-old-4","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000","registry_revision":3,"created_ms":1,"updated_ms":2},"updated_ms":9}"#;
        let receipt = parse_receipt(
            good,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .expect("a valid journal reads");
        assert_eq!(
            receipt.current,
            vec![PathBuf::from("/r")],
            "the journal's own root is current while the move is in progress"
        );

        // Every document below is a malformed journal: an empty object (present
        // but carrying none of the facts), an unknown phase, a generation its
        // phase cannot have, an unsorted member set, a relative stage path, a
        // digest that is not a SHA-256, a root without its nonce, and an invented
        // field. Each makes the whole receipt divergent, never an empty complete
        // one.
        let malformed: [&str; 7] = [
            "{}",
            r#"{"generation":4,"phase":"bogus","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["debug"],"link":"/c/target","stage":"/s","backup":"/b","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            r#"{"generation":1,"phase":"staged","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["debug"],"link":"/c/target","stage":"/s","backup":"/b","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            r#"{"generation":4,"phase":"staged","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["release","debug"],"link":"/c/target","stage":"/s","backup":"/b","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            r#"{"generation":4,"phase":"staged","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["debug"],"link":"/c/target","stage":"relative","backup":"/b","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            r#"{"generation":4,"phase":"staged","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["debug"],"link":"/c/target","stage":"/s","backup":"/b","prior_receipt_sha":"not-a-digest"}"#,
            r#"{"generation":4,"phase":"staged","root":{"kind":"external","root":"/r","generation":3},"members":["debug"],"link":"/c/target","stage":"/s","backup":"/b","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000","pinned":"/r"}"#,
        ];
        for (index, journal) in malformed.iter().enumerate() {
            let document = format!(
                r#"{{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":3,"state":"committed","layout":"whole_target","links":[{{"member":"target"}}],"transition":{journal},"updated_ms":9}}"#
            );
            assert!(
                parse_receipt(
                    document.as_bytes(),
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "malformed journal {index} was accepted: {document}"
            );
        }
    }

    #[test]
    fn every_pinned_journal_field_is_required() {
        // The review's own counterfactual: removing any one of the eight fields
        // Oxide pins on a migration journal must be refused, never read as a
        // complete receipt.
        let pinned = [
            ("link_text", r#""/r""#),
            ("link_dev", "1"),
            ("link_ino", "2"),
            ("parent_dev", "1"),
            ("parent_ino", "2"),
            ("registry_revision", "3"),
            ("created_ms", "1"),
            ("updated_ms", "2"),
        ];
        let valid = serde_json::json!({
            "version": "oxide.outputs.transition/3",
            "checkout": "/c",
            "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "generation": 3,
            "state": "committed",
            "layout": "whole_target",
            "links": [{"member": "target"}],
            "transition": {
                "generation": 4,
                "phase": "staged",
                "root": {"kind": "external", "root": "/r", "nonce": "00112233445566778899aabbccddeeff", "generation": 3},
                "members": ["debug", "release"],
                "link": "/c/target",
                "link_text": "/r",
                "link_dev": 1,
                "link_ino": 2,
                "parent_dev": 1,
                "parent_ino": 2,
                "stage": "/c/.target.oxide-new-4",
                "backup": "/c/.target.oxide-old-4",
                "prior_receipt_sha": "0000000000000000000000000000000000000000000000000000000000000000",
                "registry_revision": 3,
                "created_ms": 1,
                "updated_ms": 2
            },
            "updated_ms": 9
        });
        let bytes = serde_json::to_vec(&valid).expect("the fixture encodes");
        let receipt = parse_receipt(
            &bytes,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .expect("the complete journal reads");
        assert_eq!(receipt.current, vec![PathBuf::from("/r")]);
        let _ = pinned;
        for field in [
            "link_text",
            "link_dev",
            "link_ino",
            "parent_dev",
            "parent_ino",
            "registry_revision",
            "created_ms",
            "updated_ms",
        ] {
            let mut document = valid.clone();
            document["transition"]
                .as_object_mut()
                .expect("a journal object")
                .remove(field);
            let bytes = serde_json::to_vec(&document).expect("the fixture encodes");
            assert!(
                parse_receipt(
                    &bytes,
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "a journal without `{field}` was accepted as complete"
            );
        }
        // The pinned shapes are checked too: a digest that is not a SHA-256, a
        // zero link identity and an unusable old marker pin are each refused.
        for (field, value) in [
            ("prior_receipt_sha", serde_json::json!("not-a-digest")),
            ("link_dev", serde_json::json!(0)),
            (
                "marker_pin",
                serde_json::json!({"dev": 1, "ino": 2, "sha256": "zz"}),
            ),
            (
                "marker_pin",
                serde_json::json!({"dev": 1, "ino": 2, "sha256": "aa", "invented": 1}),
            ),
        ] {
            let mut document = valid.clone();
            document["transition"][field] = value;
            let bytes = serde_json::to_vec(&document).expect("the fixture encodes");
            assert!(
                parse_receipt(
                    &bytes,
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "a journal with an unusable `{field}` was accepted"
            );
        }
    }

    #[test]
    fn every_pinned_prune_journal_field_is_required() {
        // The same for a registry's unfinished prune intent: the four fields Oxide
        // pins are required, and its root identity must be usable.
        let valid = serde_json::json!({
            "version": "oxide.outputs.registry/2",
            "schema": 2,
            "revision": 2,
            "complete": true,
            "created_by": "oxide",
            "rows": [],
            "intents": [{
                "version": "oxide.outputs.prune-intent/1",
                "id": "11111111111111111111111111111111",
                "identity": {
                    "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    "checkout": "/c",
                    "receipt": "/r",
                    "member": "target",
                    "root": "/o",
                    "root_kind": "external",
                    "generation": 1,
                    "dev": 7,
                    "ino": 42,
                    "marker_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                },
                "phase": "renamed",
                "quarantine": "/q",
                "preview_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                "created_ms": 1,
                "updated_ms": 2
            }]
        });
        let bytes = serde_json::to_vec(&valid).expect("the fixture encodes");
        assert!(parse_registry(&bytes).is_ok(), "the complete intent reads");
        for field in ["quarantine", "preview_sha256", "created_ms", "updated_ms"] {
            let mut document = valid.clone();
            document["intents"][0]
                .as_object_mut()
                .expect("an intent object")
                .remove(field);
            let bytes = serde_json::to_vec(&document).expect("the fixture encodes");
            assert!(
                parse_registry(&bytes).is_err(),
                "an intent without `{field}` was accepted as complete"
            );
        }
        for (field, value) in [
            ("preview_sha256", serde_json::json!("00")),
            ("quarantine", serde_json::json!("relative")),
        ] {
            let mut document = valid.clone();
            document["intents"][0][field] = value;
            let bytes = serde_json::to_vec(&document).expect("the fixture encodes");
            assert!(
                parse_registry(&bytes).is_err(),
                "an intent with an unusable `{field}` was accepted"
            );
        }
        let mut document = valid;
        document["intents"][0]["identity"]["dev"] = serde_json::json!(0);
        let bytes = serde_json::to_vec(&document).expect("the fixture encodes");
        assert!(
            parse_registry(&bytes).is_err(),
            "an intent with an unusable root identity was accepted"
        );
    }

    #[test]
    fn an_unknown_prune_intent_version_is_divergent() {
        // The intent states its own version, and the only one this build reads is
        // `oxide.outputs.prune-intent/1`. A version it does not know is a
        // divergence, not a settled move.
        let intent = |version: &str| {
            serde_json::json!({
                "version": version,
                "id": "11111111111111111111111111111111",
                "identity": {
                    "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    "checkout": "/c",
                    "receipt": "/r",
                    "member": "target",
                    "root": "/o",
                    "root_kind": "external",
                    "generation": 1,
                    "dev": 7,
                    "ino": 42,
                    "marker_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                },
                "phase": "renamed",
                "quarantine": "/q",
                "preview_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                "created_ms": 1,
                "updated_ms": 2
            })
        };
        let registry = |version: &str| {
            serde_json::json!({
                "version": "oxide.outputs.registry/2",
                "schema": 2,
                "revision": 1,
                "complete": true,
                "created_by": "oxide",
                "rows": [],
                "intents": [intent(version)]
            })
        };
        let known = serde_json::to_vec(&registry(OXIDE_INTENT_V1)).expect("the fixture encodes");
        assert!(parse_registry(&known).is_ok(), "the pinned version reads");
        let unknown =
            serde_json::to_vec(&registry("oxide.outputs.prune-intent/99")).expect("encodes");
        assert!(
            parse_registry(&unknown).is_err(),
            "an unknown intent version was accepted as complete"
        );
    }

    #[test]
    fn an_unknown_root_kind_is_divergent_everywhere_it_can_appear() {
        // Oxide's own document accepts exactly `external` and `local`. A kind
        // outside that set is a document this build does not understand, and it
        // must be refused wherever a root kind is stated: a registry row, its
        // prune intent, a receipt selection, a history entry, a prepared
        // placement, a reservation and a migration journal's root.
        assert!(check_root_kind("a registry row", "external").is_ok());
        assert!(check_root_kind("a registry row", "local").is_ok());
        assert!(check_root_kind("a registry row", "future_kind").is_err());

        let registry_row = |kind: &str| {
            serde_json::json!({
                "version": "oxide.outputs.registry/2",
                "schema": 2,
                "revision": 1,
                "complete": true,
                "created_by": "oxide",
                "rows": [{
                    "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    "checkout": "/c",
                    "receipt": "/r",
                    "member": "target",
                    "root": "/o",
                    "root_kind": kind,
                    "recipe": "target-debug-v1",
                    "generation": 1,
                    "selected": false,
                    "pending": false
                }]
            })
        };
        let known = serde_json::to_vec(&registry_row("external")).expect("encodes");
        assert!(parse_registry(&known).is_ok());
        let unknown = serde_json::to_vec(&registry_row("future_kind")).expect("encodes");
        assert!(
            parse_registry(&unknown).is_err(),
            "a registry row of an unknown root kind was accepted"
        );

        let intent_registry = |kind: &str| {
            serde_json::json!({
                "version": "oxide.outputs.registry/2",
                "schema": 2,
                "revision": 1,
                "complete": true,
                "created_by": "oxide",
                "rows": [],
                "intents": [{
                    "version": OXIDE_INTENT_V1,
                    "id": "11111111111111111111111111111111",
                    "identity": {
                        "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                        "checkout": "/c",
                        "receipt": "/r",
                        "member": "target",
                        "root": "/o",
                        "root_kind": kind,
                        "generation": 1,
                        "dev": 7,
                        "ino": 42,
                        "marker_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    },
                    "phase": "renamed",
                    "quarantine": "/q",
                    "preview_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "created_ms": 1,
                    "updated_ms": 2
                }]
            })
        };
        let known = serde_json::to_vec(&intent_registry("local")).expect("encodes");
        assert!(parse_registry(&known).is_ok());
        let unknown = serde_json::to_vec(&intent_registry("future_kind")).expect("encodes");
        assert!(
            parse_registry(&unknown).is_err(),
            "a prune intent of an unknown root kind was accepted"
        );

        // Every receipt site, one at a time.
        let receipt = |what: &str, kind: &str| {
            let selected = serde_json::json!({
                "kind": kind,
                "root": "/r",
                "nonce": "00112233445566778899aabbccddeeff",
                "generation": 1
            });
            let mut document = serde_json::json!({
                "version": "oxide.outputs.transition/2",
                "checkout": "/c",
                "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "generation": 2,
                "state": "committed",
                "links": [{"member": "target", "selected": selected}],
                "roots": [],
                "updated_ms": 9
            });
            match what {
                "history" => {
                    document["links"] = serde_json::json!([{"member": "target"}]);
                    document["roots"] = serde_json::json!([{
                        "kind": kind,
                        "root": "/old",
                        "generation": 1
                    }]);
                }
                "prepared" => {
                    document["links"] = serde_json::json!([{
                        "member": "target",
                        "prepared": {
                            "generation": 3,
                            "to": {"kind": kind, "root": "/new", "generation": 3},
                            "link": "/c/target"
                        }
                    }]);
                }
                "reservation" => {
                    document["links"] = serde_json::json!([{"member": "target"}]);
                    document["reserved"] = serde_json::json!({
                        "id": "r1",
                        "nonce": "00112233445566778899aabbccddeeff",
                        "kind": kind,
                        "root": "/future",
                        "generation": 3,
                        "created_ms": 1
                    });
                }
                _ => {}
            }
            serde_json::to_vec(&document).expect("encodes")
        };
        for what in ["selection", "history", "prepared", "reservation"] {
            let known = receipt(what, "external");
            assert!(
                parse_receipt(
                    &known,
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_ok(),
                "a valid `{what}` did not read"
            );
            let unknown = receipt(what, "future_kind");
            assert!(
                parse_receipt(
                    &unknown,
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "a receipt `{what}` of an unknown root kind was accepted"
            );
        }

        // A migration journal's own root.
        let journal = |kind: &str| {
            serde_json::json!({
                "version": "oxide.outputs.transition/3",
                "checkout": "/c",
                "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "generation": 3,
                "state": "committed",
                "layout": "whole_target",
                "links": [{"member": "target"}],
                "transition": {
                    "generation": 4,
                    "phase": "staged",
                    "root": {"kind": kind, "root": "/r", "nonce": "00112233445566778899aabbccddeeff", "generation": 3},
                    "members": ["debug"],
                    "link": "/c/target",
                    "link_text": "/r",
                    "link_dev": 1,
                    "link_ino": 2,
                    "parent_dev": 1,
                    "parent_ino": 2,
                    "stage": "/s",
                    "backup": "/b",
                    "prior_receipt_sha": "0000000000000000000000000000000000000000000000000000000000000000",
                    "registry_revision": 3,
                    "created_ms": 1,
                    "updated_ms": 2
                },
                "updated_ms": 9
            })
        };
        let known = serde_json::to_vec(&journal("external")).expect("encodes");
        assert!(parse_receipt(
            &known,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        )
        .is_ok());
        let unknown = serde_json::to_vec(&journal("future_kind")).expect("encodes");
        assert!(
            parse_receipt(
                &unknown,
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
            )
            .is_err(),
            "a migration journal root of an unknown kind was accepted"
        );
    }

    #[test]
    fn an_unknown_marker_kind_is_not_understood_ownership() {
        // The root's own Oxide marker states its kind, and only Oxide's two kinds
        // are understood. A marker of an unknown kind is unreadable, which makes
        // the reference set it belongs to incomplete rather than treating the root
        // as owned evidence.
        let fixture = crate::testkit::TempDir::new("oxide-marker-kind");
        let platform = crate::os::platform();
        let marker = |kind: &str| {
            serde_json::json!({
                "version": OXIDE_MARKER_V2,
                "checkout": "/Users/fixture/Dev/example",
                "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "label": "example",
                "recipe": "target-debug-v1",
                "kind": kind,
                "volume_uuid": "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
                "nonce": "00112233445566778899aabbccddeeff",
                "checkout_volume_uuid": "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
                "generation": 1,
                "created_ms": 1_000,
            })
        };
        for (kind, readable) in [("external", true), ("local", true), ("future_kind", false)] {
            let dir = fixture.join(kind);
            std::fs::create_dir_all(&dir).expect("the fixture root");
            let path = dir.join(OXIDE_MARKER_FILE);
            std::fs::write(
                &path,
                serde_json::to_vec(&marker(kind)).expect("the marker encodes"),
            )
            .expect("the marker is written");
            std::fs::set_permissions(
                &path,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
            )
            .expect("the marker is private");
            let handle = platform.open_path(&dir).expect("the fixture root opens");
            let found = read_oxide_marker(&platform, &handle);
            assert_eq!(found.is_ok(), readable, "kind `{kind}`: {found:?}");
        }
    }

    #[test]
    fn an_uninspectable_or_changed_receipt_census_is_unknown() {
        // A folder that exists and is not a directory is unknown, never absent.
        let fixture = crate::testkit::TempDir::new("receipt-census");
        let file = fixture.write("outputs", b"not a directory");
        let deadline = Deadline::after(REFERENCE_WALL);
        assert!(matches!(
            read_receipts(&file, deadline),
            OxideReceipts::Unknown { .. }
        ));
        // A folder that is not there is established absence: nothing to read.
        assert!(matches!(
            read_receipts(&fixture.join("absent"), deadline),
            OxideReceipts::Absent
        ));
        // Nothing listed reads as a complete, empty census.
        let empty = fixture.join("empty");
        std::fs::create_dir(&empty).expect("the empty folder");
        assert!(matches!(
            read_receipts(&empty, deadline),
            OxideReceipts::Read(receipts) if receipts.is_empty()
        ));
        // A receipt that was listed and is gone by the time it is read changed the
        // census under the observation: unknown, never an empty complete one.
        let listed = fixture.join("listed");
        std::fs::create_dir(&listed).expect("the folder");
        let valid = serde_json::to_vec(&serde_json::json!({
            "version": OXIDE_RECEIPT_V2,
            "checkout": "/c",
            "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "generation": 1,
            "state": "committed",
            "links": [{"member": "target"}],
            "roots": [],
            "updated_ms": 9
        }))
        .expect("the fixture encodes");
        std::fs::write(
            listed.join("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc.json"),
            &valid,
        )
        .expect("the receipt");
        assert!(matches!(
            read_receipts(&listed, deadline),
            OxideReceipts::Read(receipts) if receipts.len() == 1
        ));
        // An overdue deadline is unknown even for an empty folder and for a folder
        // whose entries all read: the census only reports completeness inside its own
        // bound.
        let expired = Deadline::after(Duration::ZERO);
        assert!(
            matches!(
                read_receipts(&empty, expired),
                OxideReceipts::Unknown { .. }
            ),
            "an overdue empty census was reported complete"
        );
        assert!(
            matches!(
                read_receipts(&listed, expired),
                OxideReceipts::Unknown { .. }
            ),
            "an overdue census with entries was reported complete"
        );
        let vanished =
            |_path: &Path,
             _id: &str,
             _deadline: Deadline|
             -> std::result::Result<Option<OxideReceiptReference>, String> { Ok(None) };
        assert!(
            matches!(
                read_receipts_with(&listed, deadline, &vanished),
                OxideReceipts::Unknown { .. }
            ),
            "a listed receipt that vanished was read as an empty complete census"
        );
    }

    #[test]
    fn a_marker_that_disappears_after_it_was_observed_is_unknown() {
        // The marker is observed, then removed before its bytes are read: a change
        // under the observation, never "this root carries no Oxide marker".
        let fixture = crate::testkit::TempDir::new("marker-vanishes");
        let dirs = crate::testkit::Faults::default();
        std::fs::write(
            fixture.join(OXIDE_MARKER_FILE),
            serde_json::to_vec(&serde_json::json!({
                "version": OXIDE_MARKER_V2,
                "checkout": "/c",
                "checkout_id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "label": "example",
                "recipe": "target-debug-v1",
                "kind": "external",
                "generation": 1,
                "created_ms": 1_000,
            }))
            .expect("the marker encodes"),
        )
        .expect("the marker is written");
        let handle = dirs.open_path(fixture.path()).expect("the root opens");
        // Readable while nothing races it.
        assert!(matches!(read_oxide_marker(&dirs, &handle), Ok(Some(_))));
        dirs.vanishing_on_read(OXIDE_MARKER_FILE);
        let found = read_oxide_marker(&dirs, &handle);
        assert!(
            found.is_err(),
            "a marker that vanished between observation and read was read as absent: {found:?}"
        );
    }

    #[test]
    fn a_reference_observation_outside_its_bound_is_unknown() {
        // The bound covers discovery and the registry and receipts alike, and nothing
        // is concluded outside it — not even "there is no control directory" or "the
        // receipts folder is absent", because a look that ran out of time did not see
        // the whole world.
        let fixture = crate::testkit::TempDir::new("reference-bound");
        let account = crate::testkit::FakeAccount {
            home: fixture.path().to_path_buf(),
        };
        let expired = OxideSnapshot::read_within(&account, Duration::ZERO);
        assert!(!expired.complete(), "{expired:?}");
        assert!(expired.detail.is_some());
        assert!(expired.control.is_none(), "nothing was discovered");

        // With a control directory and an `outputs/` folder present, an expired bound
        // still leaves both readings unknown rather than absent or empty.
        let outputs = fixture
            .path()
            .join("Library/Caches/oxide/build-pool")
            .join(crate::inspect::OXIDE_CONTROL_DIR)
            .join(OXIDE_OUTPUTS_DIR);
        std::fs::create_dir_all(&outputs).expect("the outputs folder");
        let expired = OxideSnapshot::read_within(&account, Duration::ZERO);
        assert!(!expired.complete(), "{expired:?}");
        assert!(matches!(expired.registry, OxideRegistry::Unknown { .. }));
        assert!(matches!(expired.receipts, OxideReceipts::Unknown { .. }));

        // The same account inside a real bound reads completely.
        let read = OxideSnapshot::read_within(&account, REFERENCE_WALL);
        assert!(read.complete(), "{read:?}");
        assert!(matches!(read.registry, OxideRegistry::Absent));
        assert!(matches!(read.receipts, OxideReceipts::Read(_)));
    }

    #[test]
    fn a_record_at_another_mount_names_the_same_root() {
        // A reference recorded before a mount-point change carries the old absolute
        // path, while the root is remount-stable: the mount-relative path is the
        // identity, so the record still names this root.
        let live = Path::new("/Volumes/Builds/pool/0123456789abcdef0123456789abcdef");
        let relative = "pool/0123456789abcdef0123456789abcdef";
        // The same path.
        assert!(records_root(live, live, relative));
        // The old mount of the same relative root.
        assert!(records_root(
            Path::new("/Volumes/Builds 1/pool/0123456789abcdef0123456789abcdef"),
            live,
            relative
        ));
        // Another relative root, or a path outside the mount parent, names no root
        // here.
        assert!(!records_root(
            Path::new("/Volumes/Builds 1/pool/ffffffffffffffffffffffffffffffff"),
            live,
            relative
        ));
        assert!(!records_root(
            Path::new("/Users/tom/Dev/example/pool/0123456789abcdef0123456789abcdef"),
            live,
            relative
        ));
        assert!(!records_root(
            Path::new("/Volumes/Builds 1"),
            live,
            relative
        ));
    }

    #[test]
    fn a_candidate_lookup_that_consumes_the_bound_is_unknown() {
        // A single candidate metadata lookup can consume the whole reference bound.
        // The discovery must then be unknown — never the complete "no control
        // directory" answer a finished look may give.
        let fixture = crate::testkit::TempDir::new("discovery-bound");
        let account = crate::testkit::FakeAccount {
            home: fixture.path().to_path_buf(),
        };
        let slow = |_path: &Path| {
            std::thread::sleep(Duration::from_millis(80));
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        let deadline = Deadline::after(Duration::from_millis(30));
        let found = control_directory_with(&account, deadline, &slow);
        assert!(
            found.is_err(),
            "an overdue candidate lookup concluded no control directory: {found:?}"
        );

        // The same lookup inside a bound that can meet it is the ordinary answer.
        let quick = |_path: &Path| Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let deadline = Deadline::after(REFERENCE_WALL);
        match control_directory_with(&account, deadline, &quick) {
            Ok(None) => {}
            other => panic!("a complete look with no outputs concluded {other:?}"),
        }
    }

    #[test]
    fn a_receipt_holds_the_placements_its_layout_names() {
        // Whole target: exactly one `target` placement (a version-2 receipt).
        for placements in ["[]", r#"[{"member":"debug"}]"#] {
            let document = format!(
                r#"{{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","links":{placements},"roots":[],"updated_ms":9}}"#
            );
            assert!(
                parse_receipt(
                    document.as_bytes(),
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "a whole-target receipt without its `target` placement was accepted"
            );
        }
        // Profile: at least one placement, unique and sorted by member.
        for placements in [
            "[]",
            r#"[{"member":"release"},{"member":"debug"}]"#,
            r#"[{"member":"debug"},{"member":"debug"}]"#,
        ] {
            let document = format!(
                r#"{{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","layout":"profile","links":{placements},"roots":[],"updated_ms":9}}"#
            );
            assert!(
                parse_receipt(
                    document.as_bytes(),
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                )
                .is_err(),
                "a profile receipt with an unusable placement set was accepted"
            );
        }
        // The sorted, unique profile set reads.
        let good = br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","generation":1,"state":"committed","layout":"profile","links":[{"member":"debug"},{"member":"release"}],"roots":[],"updated_ms":9}"#;
        assert!(parse_receipt(
            good,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        )
        .is_ok());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod writer_custody_tests {
    use super::*;
    use crate::pool_custody::PoolCustody;
    use crate::testkit::TempDir;
    use std::time::Duration;

    const CHECKOUT: &str = "/work/app";

    fn custody(state: &str) -> (TempDir, PoolCustody) {
        let dir = TempDir::new("writer-custody-wiring");
        let pool = dir.path().join("pool");
        std::fs::create_dir_all(pool.join("oxide-managed")).unwrap();
        std::fs::write(pool.join("oxide-managed").join("state.json"), state).unwrap();
        std::fs::write(
            pool.join("ledger.json"),
            r#"{"version":1,"store_id":"0123456789abcdef0123456789abcdef","cap_bytes":1024,"next_slot":1,"next_lease":1,"slots":[]}"#,
        )
        .unwrap();
        let reading = PoolCustody::read(&[pool], Deadline::after(Duration::from_secs(10)));
        (dir, reading)
    }

    fn state_with(record: &str) -> String {
        format!(
            r#"{{"version":9,"next_id":2,"next_use":1,"tickets":[],"cleans":[],"records":[{record}],"recorded_accounting":"oxide.accounting/1","eviction_accounting":"oxide.eviction-accounting/1","slot_ceiling":"oxide.slots/256","holding":"oxide.holding/1"}}"#
        )
    }

    fn snapshot(custody: PoolCustody) -> OxideSnapshot {
        OxideSnapshot {
            control: None,
            registry: OxideRegistry::Absent,
            receipts: OxideReceipts::Absent,
            detail: None,
            custody,
        }
    }

    fn marker() -> OxideMarker {
        OxideMarker {
            checkout: PathBuf::from(CHECKOUT),
            checkout_id: "checkout-1".to_owned(),
            checkout_volume: None,
        }
    }

    fn custody_rows(set: &ReferenceSet) -> Vec<&ReferenceRow> {
        set.rows
            .iter()
            .filter(|row| row.source == OXIDE_WRITER_CUSTODY)
            .collect()
    }

    #[test]
    fn a_held_checkout_names_the_root_it_owns_as_current() {
        let running = r#"{"id":1,"identity":"i","compatibility":"c","profile":"release","checkout":"/work/app","phase":"running","process_group":null,"last_used":1}"#;
        let (_dir, reading) = custody(&state_with(running));
        let set = snapshot(reading).references_for(
            Path::new("/Volumes/Builds/oxide-outputs/app/target"),
            "oxide/outputs/app",
            Some(&marker()),
            None,
        );
        let rows = custody_rows(&set);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].current, "a held writer keeps the root");
        assert!(set.any_current());
    }

    #[test]
    fn an_unknown_checkout_makes_its_set_incomplete() {
        let lost = r#"{"id":1,"identity":"i","compatibility":"c","profile":"release","checkout":"/work/app","phase":"retired","process_group":null,"last_used":1,"lost":{"slot_id":"4","at_ms":1,"unclosed":"writers unknown"}}"#;
        let (_dir, reading) = custody(&state_with(lost));
        let set = snapshot(reading).references_for(
            Path::new("/Volumes/Builds/oxide-outputs/app/target"),
            "oxide/outputs/app",
            Some(&marker()),
            None,
        );
        assert!(!set.complete);
        assert!(set
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("writer custody of checkout")));
    }

    #[test]
    fn a_closed_checkout_adds_no_custody_row() {
        let idle = r#"{"id":1,"identity":"i","compatibility":"c","profile":"release","checkout":"/work/app","phase":"idle","process_group":null,"last_used":1}"#;
        let (_dir, reading) = custody(&state_with(idle));
        let set = snapshot(reading).references_for(
            Path::new("/Volumes/Builds/oxide-outputs/app/target"),
            "oxide/outputs/app",
            Some(&marker()),
            None,
        );
        assert!(custody_rows(&set).is_empty());
        assert!(!set
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("custody"));
    }

    #[test]
    fn a_pool_wide_unknown_makes_the_snapshot_incomplete() {
        assert!(snapshot(PoolCustody::default()).complete());
        assert!(!snapshot(PoolCustody::unknown("a pool cannot be read")).complete());
    }

    /// One private synthetic account home whose Oxide control directory holds
    /// the internal pool, the removable pool and one outputs registry image.
    struct CoordinatedFixture {
        _dir: TempDir,
        home: PathBuf,
        control: PathBuf,
    }

    /// The coordinated adoption transaction both controls record ready.
    const INIT_TRANSACTION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Oxide's registration of one removable pool, exactly its own field set.
    /// Its presence — not a directory — is what names the second control.
    fn registration_document() -> String {
        let uuid = crate::testkit::FIXTURE_UUID;
        format!(
            r#"{{"version":2,"mount_point":"/Volumes/Builds","volume_uuid":"{uuid}","container_uuid":"{uuid}","reserve_bytes":1,"cap_bytes":2,"quota_bytes":null,"capacity_bytes":null,"registered_ms":1,"mode":"ephemeral"}}"#
        )
    }

    /// The empty library ledger Oxide writes for a pool with no slot.
    const EMPTY_LEDGER: &str = r#"{"version":1,"store_id":"0123456789abcdef0123456789abcdef","cap_bytes":1024,"next_slot":1,"next_lease":1,"slots":[]}"#;

    impl CoordinatedFixture {
        fn new(label: &str) -> Self {
            let dir = TempDir::new(label);
            let home = dir.path().join("home");
            let control = home
                .join("Library/Caches/oxide/build-pool")
                .join(crate::inspect::OXIDE_CONTROL_DIR);
            std::fs::create_dir_all(control.join(OXIDE_OUTPUTS_DIR)).unwrap();
            Self {
                _dir: dir,
                home,
                control,
            }
        }

        fn internal(&self) -> PathBuf {
            self.control
                .parent()
                .expect("the control directory is inside a pool")
                .to_path_buf()
        }

        fn removable(&self) -> PathBuf {
            self.control.join(REMOVABLE_POOL_DIR)
        }

        /// A version-9 control state carrying `fence` and its raw
        /// initialization member(s), exactly as Oxide serializes them.
        fn state(fence: &str, init: &str) -> String {
            format!(
                r#"{{"version":9,"next_id":1,"next_use":1,"tickets":[],"cleans":[],"records":[],"owned_outputs":"{fence}"{init},"recorded_accounting":"oxide.accounting/1","eviction_accounting":"oxide.eviction-accounting/1","slot_ceiling":"oxide.slots/256","holding":"oxide.holding/1"}}"#
            )
        }

        fn write_pool(&self, pool: &Path, fence: &str, init: &str) {
            let control = pool.join(crate::pool_custody::CONTROL_DIR);
            std::fs::create_dir_all(&control).unwrap();
            std::fs::write(
                control.join(crate::pool_custody::STATE_FILE),
                Self::state(fence, init),
            )
            .unwrap();
            std::fs::write(pool.join(crate::pool_custody::LEDGER_FILE), EMPTY_LEDGER).unwrap();
        }

        /// The exact emitted adoption: both controls `/5` ready, one
        /// transaction.
        fn coordinated_pair(&self) {
            let ready = format!(
                r#","outputs_init":{{"ready":{{"transaction":"{INIT_TRANSACTION}","ready_ms":1}}}}"#
            );
            for pool in [self.internal(), self.removable()] {
                self.write_pool(
                    &pool,
                    crate::pool_custody::COORDINATED_OWNED_OUTPUTS_FENCE,
                    &ready,
                );
            }
        }

        /// Oxide's registration of one removable pool, exactly its own field set.
        /// Its presence — not a directory — is what names the second control.
        fn register_peer(&self) {
            let document = registration_document();
            std::fs::write(
                self.control.join(crate::inspect::OXIDE_REGISTRATION_FILE),
                document,
            )
            .unwrap();
        }

        /// The predecessor `/4` pair with its own completed record, as the
        /// published class writes it.
        fn routed_pair(&self) {
            let ready = format!(
                r#","outputs_init":{{"ready":{{"transaction":"{INIT_TRANSACTION}","ready_ms":1}}}}"#
            );
            for pool in [self.internal(), self.removable()] {
                self.write_pool(
                    &pool,
                    crate::pool_custody::ROUTED_OWNED_OUTPUTS_FENCE,
                    &ready,
                );
            }
        }

        fn registry(&self, version: &str, schema: u32, created_by: &str, settled: bool) {
            let settled = if settled { r#","settled":[]"# } else { "" };
            let document = format!(
                r#"{{"version":"{version}","schema":{schema},"revision":1,"complete":true,"created_by":"{created_by}","rows":[]{settled}}}"#
            );
            std::fs::write(
                self.control
                    .join(OXIDE_OUTPUTS_DIR)
                    .join(OXIDE_REGISTRY_FILE),
                document,
            )
            .unwrap();
        }

        fn snapshot(&self) -> OxideSnapshot {
            OxideSnapshot::read_within(
                &crate::testkit::FakeAccount {
                    home: self.home.clone(),
                },
                Duration::from_secs(10),
            )
        }
    }

    #[test]
    fn the_snapshot_binds_the_coordinated_control_pair_to_its_registry() {
        // The registered pair and the `/4` image it created are one handoff:
        // the snapshot reads both, and an unrelated root's set is complete and
        // unreferenced.
        let fixture = CoordinatedFixture::new("coordinated-bound-pair");
        fixture.coordinated_pair();
        fixture.registry(OXIDE_REGISTRY_V4, 4, INIT_TRANSACTION, true);
        let snapshot = fixture.snapshot();
        assert!(snapshot.complete(), "{snapshot:?}");
        match &snapshot.registry {
            OxideRegistry::Read {
                version,
                created_by,
                ..
            } => {
                assert_eq!(version, OXIDE_REGISTRY_V4);
                assert_eq!(created_by, INIT_TRANSACTION);
            }
            other => panic!("the coordinated image read as {other:?}"),
        }
        let set = fixture.snapshot().references_for(
            Path::new("/Volumes/Builds/oxide-outputs/app/target"),
            "oxide/outputs/app",
            None,
            None,
        );
        assert!(set.complete, "{set:?}");
        assert!(!set.any_current());
    }

    #[test]
    fn a_mixed_or_one_sided_coordinated_adoption_is_unknown() {
        // The pair records one transaction and the image another: the ready
        // handoff and the image are different generations.
        let fixture = CoordinatedFixture::new("coordinated-mixed-transaction");
        fixture.coordinated_pair();
        fixture.registry(
            OXIDE_REGISTRY_V4,
            4,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            true,
        );
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert!(
            matches!(&snapshot.registry, OxideRegistry::Unknown { reason } if reason.contains("not the completed coordinated transaction")),
            "{snapshot:?}"
        );
        let set = snapshot.references_for(
            Path::new("/Volumes/Builds/oxide-outputs/app/target"),
            "oxide/outputs/app",
            None,
            None,
        );
        assert!(!set.complete, "an unbound image keeps every root");

        // A completed pair whose image is still the predecessor version.
        let fixture = CoordinatedFixture::new("coordinated-predecessor-image");
        fixture.coordinated_pair();
        fixture.registry(OXIDE_REGISTRY_V2, 2, INIT_TRANSACTION, false);
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert!(
            matches!(&snapshot.registry, OxideRegistry::Unknown { reason } if reason.contains("not oxide.outputs.registry/4")),
            "{snapshot:?}"
        );

        // A `/4` image with no coordinated controls at all: the reverse
        // one-sided adoption.
        let fixture = CoordinatedFixture::new("coordinated-image-only");
        fixture.registry(OXIDE_REGISTRY_V4, 4, INIT_TRANSACTION, true);
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert!(
            matches!(&snapshot.registry, OxideRegistry::Unknown { reason } if reason.contains("no pool control records the completed coordinated fence")),
            "{snapshot:?}"
        );

        // A completed pair whose image is absent is never rebuilt or guessed.
        let fixture = CoordinatedFixture::new("coordinated-image-absent");
        fixture.coordinated_pair();
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert!(
            matches!(&snapshot.registry, OxideRegistry::Unknown { reason } if reason.contains("not oxide.outputs.registry/4")),
            "{snapshot:?}"
        );

        // The published `/4` pair keeps its own historical disposition: both
        // controls and a `/2` image are a complete legacy reading.
        let fixture = CoordinatedFixture::new("coordinated-routed-legacy");
        fixture.routed_pair();
        fixture.registry(OXIDE_REGISTRY_V2, 2, INIT_TRANSACTION, false);
        let snapshot = fixture.snapshot();
        assert!(snapshot.complete(), "{snapshot:?}");
        assert!(matches!(
            &snapshot.registry,
            OxideRegistry::Read { version, .. } if version == OXIDE_REGISTRY_V2
        ));

        // A registered pair that is mid-adoption (internal `/5`, removable
        // `/4`) is a pool-wide unknown whatever the image says.
        let fixture = CoordinatedFixture::new("coordinated-one-sided-controls");
        fixture.register_peer();
        fixture.routed_pair();
        let ready = format!(
            r#","outputs_init":{{"ready":{{"transaction":"{INIT_TRANSACTION}","ready_ms":1}}}}"#
        );
        fixture.write_pool(
            &fixture.internal(),
            crate::pool_custody::COORDINATED_OWNED_OUTPUTS_FENCE,
            &ready,
        );
        fixture.registry(OXIDE_REGISTRY_V4, 4, INIT_TRANSACTION, true);
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert!(
            snapshot
                .custody
                .unattributed()
                .is_some_and(|reason| reason.contains("paired adoption is incomplete")),
            "{snapshot:?}"
        );
    }

    #[test]
    fn a_registered_peer_and_an_absent_outputs_folder_are_unknown() {
        // The registration names a second control that never wrote its state,
        // so the pair is incomplete whatever the internal control records.
        let fixture = CoordinatedFixture::new("coordinated-registered-absent-peer");
        fixture.register_peer();
        let ready = format!(
            r#","outputs_init":{{"ready":{{"transaction":"{INIT_TRANSACTION}","ready_ms":1}}}}"#
        );
        fixture.write_pool(
            &fixture.internal(),
            crate::pool_custody::COORDINATED_OWNED_OUTPUTS_FENCE,
            &ready,
        );
        fixture.registry(OXIDE_REGISTRY_V4, 4, INIT_TRANSACTION, true);
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert_eq!(snapshot.custody.pools().len(), 2, "{snapshot:?}");
        assert!(
            snapshot
                .custody
                .unattributed()
                .is_some_and(|reason| reason.contains("paired adoption is incomplete")),
            "{snapshot:?}"
        );

        // A covered control state with its `outputs` folder absent: the registry
        // image cannot have lived there, so discovery reports unknown rather
        // than "no Oxide control directory" and no references.
        let fixture = CoordinatedFixture::new("coordinated-missing-outputs");
        fixture.coordinated_pair();
        fixture.registry(OXIDE_REGISTRY_V4, 4, INIT_TRANSACTION, true);
        std::fs::remove_dir_all(fixture.control.join(OXIDE_OUTPUTS_DIR)).unwrap();
        let snapshot = fixture.snapshot();
        assert!(!snapshot.complete(), "{snapshot:?}");
        assert!(
            snapshot
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("records a covered owned-outputs fence but")),
            "{snapshot:?}"
        );
        let set = snapshot.references_for(
            Path::new("/Volumes/Builds/oxide-outputs/app/target"),
            "oxide/outputs/app",
            None,
            None,
        );
        assert!(!set.complete, "an unreadable registry keeps every root");
    }

    #[test]
    fn registration_names_the_removable_peer_and_a_legacy_entry_stays_a_peer() {
        // Registration is the coordinated pair membership, exactly as Oxide's own
        // `removable::coordinate` decides it; an extant `removable` entry stays a
        // custody peer whatever the registration says. A registration that cannot
        // be read, and an entry that is not a real directory, are unknown.
        let dir = TempDir::new("writer-custody-pools");
        let internal = dir.path().join("pool");
        let control = internal.join("oxide-managed");
        std::fs::create_dir_all(&control).unwrap();
        let removable = control.join(REMOVABLE_POOL_DIR);
        let deadline = || Deadline::after(Duration::from_secs(10));
        let registration = control.join(crate::inspect::OXIDE_REGISTRATION_FILE);

        // Nothing registered, nothing present: the internal pool alone.
        assert_eq!(
            pools_holding(&control, deadline()).unwrap(),
            vec![internal.clone()]
        );

        // A registration names the sibling even while its own directory is away.
        std::fs::write(&registration, registration_document()).unwrap();
        assert_eq!(
            pools_holding(&control, deadline()).unwrap(),
            vec![internal.clone(), removable.clone()]
        );

        // Without the registration and the entry, the peer is gone again.
        std::fs::remove_file(&registration).unwrap();
        assert_eq!(
            pools_holding(&control, deadline()).unwrap(),
            vec![internal.clone()]
        );

        // A legacy directory with no registration is still a custody peer.
        std::fs::create_dir(&removable).unwrap();
        assert_eq!(
            pools_holding(&control, deadline()).unwrap(),
            vec![internal.clone(), removable.clone()]
        );

        // A malformed registration is unknown, never skipped.
        std::fs::write(&registration, b"{}").unwrap();
        assert!(pools_holding(&control, deadline()).is_err());

        // An entry that is not a directory is unknown even without a registration.
        std::fs::remove_file(&registration).unwrap();
        std::fs::remove_dir(&removable).unwrap();
        std::fs::write(&removable, b"not a pool").unwrap();
        assert!(pools_holding(&control, deadline()).is_err());

        // An unreadable registration document (a directory) is unknown too.
        std::fs::remove_file(&removable).unwrap();
        std::fs::create_dir(&registration).unwrap();
        assert!(pools_holding(&control, deadline()).is_err());
    }

    #[test]
    fn a_legacy_removable_peer_keeps_its_writers_and_an_unregistered_pair_is_unknown() {
        // Historical disposition: a `removable` directory with no registration is
        // still read, so its live writer holds the checkout instead of reading as
        // closed. Without this the shared lookup would release a checkout whose
        // old peer's Cargo may still be writing.
        let dir = TempDir::new("writer-custody-legacy-peer");
        let internal = dir.path().join("pool");
        let control = internal.join("oxide-managed");
        let removable = control.join(REMOVABLE_POOL_DIR);
        let removable_control = removable.join("oxide-managed");
        std::fs::create_dir_all(&removable_control).unwrap();
        let running = r#"{"id":1,"identity":"i","compatibility":"c","profile":"release","checkout":"/work/app","phase":"running","process_group":null,"last_used":1}"#;
        std::fs::write(removable_control.join("state.json"), state_with(running)).unwrap();
        std::fs::write(
            removable.join("ledger.json"),
            r#"{"version":1,"store_id":"0123456789abcdef0123456789abcdef","cap_bytes":1024,"next_slot":1,"next_lease":1,"slots":[]}"#,
        )
        .unwrap();
        let deadline = || Deadline::after(Duration::from_secs(10));
        let pools = pools_holding(&control, deadline()).unwrap();
        assert_eq!(pools, vec![internal.clone(), removable.clone()]);
        let reading = PoolCustody::read(&pools, Deadline::after(Duration::from_secs(10)));
        assert!(
            matches!(reading.for_checkout(Path::new(CHECKOUT)), Custody::Held(_)),
            "{reading:?}"
        );

        // With the internal control on the coordinated fence, a peer that records
        // nothing is a one-sided adoption: unknown, so the batch is kept.
        std::fs::create_dir_all(&control).unwrap();
        let ready = format!(
            r#","outputs_init":{{"ready":{{"transaction":"{INIT_TRANSACTION}","ready_ms":1}}}}"#
        );
        let state = format!(
            r#"{{"version":9,"next_id":1,"next_use":1,"tickets":[],"cleans":[],"records":[],"owned_outputs":"{}"{ready},"recorded_accounting":"oxide.accounting/1","eviction_accounting":"oxide.eviction-accounting/1","slot_ceiling":"oxide.slots/256","holding":"oxide.holding/1"}}"#,
            crate::pool_custody::COORDINATED_OWNED_OUTPUTS_FENCE
        );
        std::fs::write(control.join("state.json"), state).unwrap();
        // The delegated lookup refuses the incomplete pair either way: a peer
        // that records no covered fence is a known not-covered, and one that
        // cannot be read is unknown. Neither is ever the covered class.
        assert!(
            !matches!(
                crate::pool_custody::owned_outputs_fence(&pools, deadline()),
                crate::pool_custody::OutputsFence::Covered { .. }
            ),
            "an unregistered legacy peer never completes a coordinated pair"
        );
        // The generic release's custody reading is the one that must keep the
        // batch, and it does: the pair is one-sided.
        let reading = PoolCustody::read(&pools, Deadline::after(Duration::from_secs(10)));
        assert!(reading.unattributed().is_some(), "{reading:?}");
    }
}
