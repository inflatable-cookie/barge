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
//!   registry `oxide.outputs.registry/2` (and its `/1` predecessor), the
//!   checkout receipts `oxide.outputs.transition/1..3`, and one root marker
//!   `oxide.outputs.root/2` (`/1` is read as unknown). Nothing is written,
//!   nothing is adopted and no Oxide fact becomes authoritative. A row that is
//!   selected, pending, reserved, pruning or cancelling is current; Oxide's own
//!   [`Row::live`] predicate includes `pending`, and this reader keeps that
//!   stricter reading so a row Oxide has not settled never releases a root.
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
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::area::{self, AreaDocument, HoldReading, PrunePolicy};
use crate::cache::{self, PlacementForm, PlacementSet};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{AccountFacts, DirHandle, DirOps, VolumeUuid};
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

/// Oxide's current outputs registry.
pub const OXIDE_REGISTRY_V2: &str = "oxide.outputs.registry/2";
/// Oxide's predecessor outputs registry, read as its own field set.
pub const OXIDE_REGISTRY_V1: &str = "oxide.outputs.registry/1";
/// Oxide's current root marker.
pub const OXIDE_MARKER_V2: &str = "oxide.outputs.root/2";
/// Oxide's predecessor root marker. Its field set predates the nonce and the
/// checkout volume, so this reader reports it as unknown rather than guessing.
pub const OXIDE_MARKER_V1: &str = "oxide.outputs.root/1";
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
        root_id: &str,
        root_path: &Path,
        holds: &area::HoldReading,
        holds_error: Option<&str>,
    ) -> ReferenceSet {
        let mut set = ReferenceSet::empty();
        if let Some(detail) = &self.placements_detail {
            set.absorb(ReferenceSet::unknown(detail.clone()));
        }
        for record in &self.placements.records {
            if !placement_names_root(record, root_id, root_path) {
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
                    "permits releasing a root past its declared age"
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
fn placement_names_root(record: &cache::PlacementRecord, root_id: &str, root_path: &Path) -> bool {
    let recorded = &record.recorded_path;
    if recorded == root_path || recorded.starts_with(root_path) || root_path.starts_with(recorded) {
        return true;
    }
    match &record.form {
        PlacementForm::HeldRoot(form) => form.held_root.root_id == root_id,
        PlacementForm::Link(form) => form.root_id == root_id,
        PlacementForm::Setting(form) => form.data_path == root_path,
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

/// One unfinished prune intent from Oxide's registry journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideIntent {
    /// The root the intent names.
    pub root: PathBuf,
    /// The journal phase it records.
    pub phase: String,
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
}

impl OxideSnapshot {
    /// No Oxide outputs control directory: no Oxide record can name a root.
    pub fn none() -> Self {
        Self {
            control: None,
            registry: OxideRegistry::Absent,
            receipts: OxideReceipts::Absent,
            detail: None,
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
        registry_complete && !matches!(self.receipts, OxideReceipts::Unknown { .. })
    }

    /// Read Oxide's reference records for this account, bounded and read-only.
    ///
    /// The control directory is found through Oxide's own pools record and its
    /// current and retired default pools, exactly as S2's observations are. A
    /// pool whose control directory holds an `outputs/` folder is the one this
    /// snapshot reads; nothing is created or repaired.
    pub fn read(account: &dyn AccountFacts) -> Self {
        let control = match control_directory(account) {
            Ok(Some(control)) => control,
            Ok(None) => return Self::none(),
            Err(reason) => {
                let reason = reason.to_string();
                return Self {
                    control: None,
                    registry: OxideRegistry::Unknown {
                        reason: reason.clone(),
                    },
                    receipts: OxideReceipts::Unknown {
                        reason: reason.clone(),
                    },
                    detail: Some(reason),
                };
            }
        };
        let deadline = Deadline::after(REFERENCE_WALL);
        let outputs = control.join(OXIDE_OUTPUTS_DIR);
        let registry = read_registry(&outputs.join(OXIDE_REGISTRY_FILE), deadline);
        let receipts = read_receipts(&outputs, deadline);
        let detail = match (&registry, &receipts) {
            (OxideRegistry::Unknown { reason }, _) => Some(reason.clone()),
            (_, OxideReceipts::Unknown { reason }) => Some(reason.clone()),
            _ => None,
        };
        Self {
            control: Some(control),
            registry,
            receipts,
            detail,
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
            } => {
                if !complete {
                    set.absorb(ReferenceSet::unknown(
                        "Oxide's outputs registry reports itself incomplete",
                    ));
                }
                for row in rows.iter().filter(|row| row_names_root(row, root_path)) {
                    let row_volume = row.checkout_volume.clone();
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
                for intent in intents.iter().filter(|intent| intent.root == root_path) {
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
                    if !receipt.roots.iter().any(|root| root == root_path) {
                        continue;
                    }
                    let current = receipt.current.iter().any(|root| root == root_path);
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
        set
    }
}

/// Whether one registry row names one root.
///
/// The recorded root path is the key. The row's nonce is carried as evidence and
/// is never compared: a Barge-owned root's nonce is Barge's, an Oxide marker's is
/// Oxide's, so comparing them would decide a fact neither record states. Path
/// equality authorises nothing on its own either — it only makes a reference row
/// current, which keeps the root.
fn row_names_root(row: &OxideRow, root_path: &Path) -> bool {
    row.root == root_path
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
fn control_directory(account: &dyn AccountFacts) -> Result<Option<PathBuf>> {
    let state_dir = account.tool_state_dir(crate::inspect::OXIDE_TOOL)?;
    let cache_dir = account.tool_cache_dir(crate::inspect::OXIDE_TOOL)?;
    let observations = crate::inspect::observe_oxide_at(&state_dir, &cache_dir);
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
        match std::fs::metadata(&outputs) {
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
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

/// Read Oxide's outputs registry strictly.
fn read_registry(path: &Path, deadline: Deadline) -> OxideRegistry {
    let bounds = Bounds::new(REGISTRY_CAP_BYTES, REGISTRY_ROWS, deadline);
    let bytes = match state::read_bounded(path, &bounds) {
        ReadOutcome::Absent => return OxideRegistry::Absent,
        ReadOutcome::Refused(error) => {
            return OxideRegistry::Unknown {
                reason: format!("{}: {error}", path.display()),
            }
        }
        ReadOutcome::Present(bytes) => bytes,
    };
    match parse_registry(&bytes) {
        Ok(reading) => reading,
        Err(error) => OxideRegistry::Unknown {
            reason: format!("{}: {error}", path.display()),
        },
    }
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
    #[serde(default)]
    rows: Vec<RawRow>,
    #[serde(default)]
    intents: Vec<RawIntent>,
    #[serde(default)]
    #[allow(dead_code)]
    cancelled_overflow: Vec<String>,
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
    #[allow(dead_code)]
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
    if raw.version != OXIDE_REGISTRY_V2 && raw.version != OXIDE_REGISTRY_V1 {
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
        if !matches!(
            intent.phase.as_str(),
            "intent" | "renamed" | "deleting" | "marker_last" | "deleted" | "resolved"
        ) {
            return Err(format!(
                "a prune intent states phase `{}`, which Oxide never writes",
                intent.phase
            ));
        }
        if intent.id.is_empty() || !intent.quarantine.is_absolute() {
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
            || !intent.identity.root.is_absolute()
            || intent.identity.marker_digest.len() != 64
            || !intent
                .identity
                .marker_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("a prune intent names an unusable root identity for the move".to_owned());
        }
        intents.push(OxideIntent {
            root: intent.identity.root,
            phase: intent.phase,
        });
    }
    if raw.revision == 0 && !rows.is_empty() {
        return Err("a non-empty registry states revision 0".to_owned());
    }
    Ok(OxideRegistry::Read {
        rows,
        intents,
        complete: raw.complete,
    })
}

/// Read every receipt present in Oxide's outputs folder.
fn read_receipts(outputs: &Path, deadline: Deadline) -> OxideReceipts {
    if !outputs.is_dir() {
        return OxideReceipts::Absent;
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
        match read_receipt(&path, checkout_id, deadline) {
            Ok(Some(receipt)) => receipts.push(receipt),
            Ok(None) => continue,
            Err(reason) => return OxideReceipts::Unknown { reason },
        }
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
    let facts = match dirs.child_facts(root, OXIDE_MARKER_FILE) {
        Ok(Some(facts)) => facts,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("the marker cannot be read: {error}")),
    };
    if facts.kind != crate::os::EntryKind::File {
        return Err("the Oxide marker is not a regular file".to_owned());
    }
    let bytes = match dirs.read_child_file(root, OXIDE_MARKER_FILE, MARKER_CAP_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("the marker cannot be read: {error}")),
    };
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
    let checkout_volume = match raw.checkout_volume_uuid.as_deref() {
        Some(raw) => Some(
            VolumeUuid::parse(raw)
                .map_err(|error| format!("the marker's checkout volume is not a UUID: {error}"))?,
        ),
        None => None,
    };
    Ok(Some(OxideMarker {
        checkout: raw.checkout,
        checkout_id: raw.checkout_id,
        checkout_volume,
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
    #[allow(dead_code)]
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

    #[test]
    fn a_registry_row_that_is_not_settled_names_its_root() {
        let row = OxideRow {
            checkout_id: "checkout-1".to_owned(),
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
        let v2 = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":3,"complete":true,"created_by":"oxide","rows":[{"checkout_id":"c1","checkout":"/Users/fixture/Dev/example","receipt":"/pool/receipt.json","member":"target","root":"/Volumes/Builds/oxide/outputs/root","root_kind":"external","recipe":"target-debug-v1","nonce":"00112233445566778899aabbccddeeff","generation":1,"root_dev":7,"root_ino":42,"marker_digest":"aa","selected":false,"pending":false}]}"#;
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
        let bytes = br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":2,"complete":true,"created_by":"oxide","rows":[],"intents":[{"version":"oxide.outputs.prune-intent/1","id":"i1","identity":{"checkout_id":"c1","checkout":"/Users/fixture/Dev/example","receipt":"/pool/outputs/c1.json","member":"target","root":"/Volumes/Builds/oxide/outputs/root","root_kind":"external","generation":1,"dev":7,"ino":42,"marker_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"phase":"renamed","quarantine":"/pool/outputs/.prune-i1","preview_sha256":"0000000000000000000000000000000000000000000000000000000000000000","created_ms":1,"updated_ms":2}]}"#;
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
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[],"intents":[{"version":"oxide.outputs.prune-intent/1","id":"i1","identity":{"checkout_id":"c1","checkout":"/c","receipt":"/r","member":"target","root":"/o","root_kind":"external","generation":1,"dev":7,"ino":42,"marker_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"phase":"bogus","quarantine":"/q","preview_sha256":"0000000000000000000000000000000000000000000000000000000000000000","created_ms":1,"updated_ms":2}]}"#,
            // An intent without its root identity.
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[],"intents":[{"version":"oxide.outputs.prune-intent/1","id":"i1","identity":{"root":"/o"},"phase":"renamed","quarantine":"/q","preview_sha256":"0000000000000000000000000000000000000000000000000000000000000000","created_ms":1,"updated_ms":2}]}"#,
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
        let v2 = br#"{"version":"oxide.outputs.transition/2","checkout":"/Users/fixture/Dev/example","checkout_id":"c1","generation":2,"state":"committed","links":[{"member":"target","selected":{"kind":"external","root":"/Volumes/Builds/oxide/outputs/current","generation":2}}],"roots":[{"kind":"external","root":"/Volumes/Builds/oxide/outputs/old","generation":1}],"updated_ms":9}"#;
        let receipt = parse_receipt(v2, "c1").expect("v2 reads");
        assert_eq!(
            receipt.current,
            vec![PathBuf::from("/Volumes/Builds/oxide/outputs/current")]
        );
        assert_eq!(receipt.roots.len(), 2, "history is named but not current");

        // A version-1 receipt states its one whole-target selection at the top.
        let v1 = br#"{"version":"oxide.outputs.transition/1","checkout":"/Users/fixture/Dev/example","checkout_id":"c1","generation":1,"state":"committed","selected":{"kind":"external","root":"/Volumes/Builds/oxide/outputs/current","generation":1},"updated_ms":9}"#;
        let legacy = parse_receipt(v1, "c1").expect("v1 reads");
        assert_eq!(legacy.current, legacy.roots);

        // A version-3 receipt reserves its next root at the root level.
        let v3 = br#"{"version":"oxide.outputs.transition/3","checkout":"/Users/fixture/Dev/example","checkout_id":"c1","generation":3,"state":"committed","layout":"profile","links":[{"member":"target"}],"reserved":{"id":"r1","nonce":"0011","kind":"external","root":"/Volumes/Builds/oxide/outputs/reserved","generation":4,"created_ms":1},"updated_ms":9}"#;
        let reserved = parse_receipt(v3, "c1").expect("v3 reads");
        assert_eq!(
            reserved.current,
            vec![PathBuf::from("/Volumes/Builds/oxide/outputs/reserved")]
        );

        let unknown = br#"{"version":"oxide.outputs.transition/9","checkout":"/Users/fixture/Dev/example","checkout_id":"c1","generation":1,"state":"committed","links":[],"roots":[],"updated_ms":9}"#;
        assert!(parse_receipt(unknown, "c1").is_err());
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
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"links":[],"roots":[],"updated_ms":9}"#,
            // A state Oxide never writes.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"finished","links":[],"roots":[],"updated_ms":9}"#,
            // No `updated_ms`.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","links":[],"roots":[]}"#,
            // A version-3 receipt without its layout.
            br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","links":[],"roots":[],"updated_ms":9}"#,
            // A version-2 receipt with a successor layout.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","layout":"profile","links":[],"roots":[],"updated_ms":9}"#,
            // A version-2 receipt with a top-level reference.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","selected":{"kind":"external","root":"/r","generation":1},"links":[],"roots":[],"updated_ms":9}"#,
            // A version-1 receipt with placements.
            br#"{"version":"oxide.outputs.transition/1","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","links":[{"member":"target"}],"roots":[],"updated_ms":9}"#,
            // A whole-target version-3 receipt without its migration journal.
            br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","layout":"whole_target","links":[],"roots":[],"updated_ms":9}"#,
            // A placement without its member.
            br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","links":[{"selected":{"kind":"external","root":"/r","generation":1}}],"roots":[],"updated_ms":9}"#,
        ];
        for (index, case) in cases.iter().enumerate() {
            assert!(
                parse_receipt(case, "c1").is_err(),
                "case {index} was accepted as a receipt: {}",
                String::from_utf8_lossy(case)
            );
        }
        // A version-3 receipt whose history entry omits its generation is
        // divergent too: Oxide always records it.
        let history = br#"{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","links":[],"roots":[{"kind":"external","root":"/r"}],"updated_ms":9}"#;
        assert!(parse_receipt(history, "c1").is_err());
    }

    #[test]
    fn a_migration_journal_is_read_strictly_and_names_the_root_it_keeps() {
        // A whole-target version-3 receipt carries the journal of the move in
        // progress, and the root that journal names is the one the checkout keeps.
        let good = br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"c1","generation":3,"state":"committed","layout":"whole_target","links":[{"member":"target"}],"transition":{"generation":4,"phase":"staged","root":{"kind":"external","root":"/r","nonce":"00112233445566778899aabbccddeeff","generation":3},"members":["debug","release"],"link":"/c/target","link_text":"/r","link_dev":1,"link_ino":2,"parent_dev":1,"parent_ino":2,"stage":"/c/.target.oxide-new-4","backup":"/c/.target.oxide-old-4","prior_receipt_sha":"0000000000000000000000000000000000000000000000000000000000000000","registry_revision":3,"created_ms":1,"updated_ms":2},"updated_ms":9}"#;
        let receipt = parse_receipt(good, "c1").expect("a valid journal reads");
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
                r#"{{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"c1","generation":3,"state":"committed","layout":"whole_target","links":[{{"member":"target"}}],"transition":{journal},"updated_ms":9}}"#
            );
            assert!(
                parse_receipt(document.as_bytes(), "c1").is_err(),
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
            "checkout_id": "c1",
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
        let receipt = parse_receipt(&bytes, "c1").expect("the complete journal reads");
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
                parse_receipt(&bytes, "c1").is_err(),
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
                parse_receipt(&bytes, "c1").is_err(),
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
                "id": "i1",
                "identity": {
                    "checkout_id": "c1",
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
    fn a_receipt_holds_the_placements_its_layout_names() {
        // Whole target: exactly one `target` placement (a version-2 receipt).
        for placements in ["[]", r#"[{"member":"debug"}]"#] {
            let document = format!(
                r#"{{"version":"oxide.outputs.transition/2","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","links":{placements},"roots":[],"updated_ms":9}}"#
            );
            assert!(
                parse_receipt(document.as_bytes(), "c1").is_err(),
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
                r#"{{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","layout":"profile","links":{placements},"roots":[],"updated_ms":9}}"#
            );
            assert!(
                parse_receipt(document.as_bytes(), "c1").is_err(),
                "a profile receipt with an unusable placement set was accepted"
            );
        }
        // The sorted, unique profile set reads.
        let good = br#"{"version":"oxide.outputs.transition/3","checkout":"/c","checkout_id":"c1","generation":1,"state":"committed","layout":"profile","links":[{"member":"debug"},{"member":"release"}],"roots":[],"updated_ms":9}"#;
        assert!(parse_receipt(good, "c1").is_ok());
    }
}
