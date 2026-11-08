//! Effect-free case specifications and owned fixture preparation for the
//! limited first batch (`docs/knowledge/contracts/o2-tape.md`, "First-batch
//! fixture cases").
//!
//! The specification is data: a case id, its family, the deliberate state it
//! creates, the in-window actions it does **not** perform here, the premises it
//! still needs, the decision sites it obliges, its declared fates and its CF
//! applicability with a reason for every non-applicable CF. No specification
//! holds a byte, a device number or an inode: those are observed when the state
//! is materialized, and only then serialized.
//!
//! Preparation is separate from the specification and from any window. It builds
//! the shared owned setup once (`crate::tape_harness::build_fixture_setup`), writes
//! only the case's declared paths inside the fixture, observes each of them
//! (`lstat`, its parent's own identity, its real bytes digest) and records the
//! declared state, the owned roots and the case manifest. It opens no tape window,
//! calls no `release::plan`, and evaluates nothing: the end-to-end engine behavior
//! of every case remains unrun and is not claimed by these tests.
//!
//! Two kinds of case are refused by name rather than approximated:
//!
//! - an **unsupported** case needs fixture evidence this preparation may not
//!   produce (a second mount epoch, a real duplicate-UUID mount census, an
//!   internal boot-Data placement, or a registry that exceeds the shared caps);
//! - a **deferred** case belongs to the deliberately held second batch (writers
//!   and custody, admission effects).
//!
//! Neither ever falls back to the BASE state: [`prepare`] refuses, and the plan
//! prints the named reason.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::area::PrunePolicy;
use crate::inspect::OXIDE_CONTROL_DIR;
use crate::os::VolumeUuid;
use crate::pool_custody::{CONTROL_DIR, STATE_FILE};
use crate::reference::{
    OXIDE_INTENT_V1, OXIDE_MARKER_FILE, OXIDE_MARKER_V1, OXIDE_MARKER_V2, OXIDE_OUTPUTS_DIR,
    OXIDE_RECEIPT_V1, OXIDE_RECEIPT_V2, OXIDE_REGISTRY_FILE, OXIDE_REGISTRY_V2, REGISTRY_CAP_BYTES,
};
use crate::tape_harness::{manifest_document, FixtureSetup};

/// The effect-free plan document. It is a diagnostic print, never a capture, a
/// transcript or a manifest, and it is never proof input.
pub(crate) const PLAN_DOCUMENT: &str = "barge.capture.fixture-plan/1";

/// The fixture's checkout id: the same 64-hex id the BASE fixture uses.
pub(crate) const CHECKOUT_ID: &str =
    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
/// The second checkout id, for the contradictory registry case.
pub(crate) const OTHER_CHECKOUT_ID: &str =
    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
/// The nonce the fixture's records and markers carry.
pub(crate) const NONCE: &str = "00112233445566778899aabbccddeeff";
/// A second nonce, recorded by one document where the other records [`NONCE`].
pub(crate) const OTHER_NONCE: &str = "ffeeddccbbaa99887766554433221100";
/// The pending intent's 128-bit id.
pub(crate) const INTENT_ID: &str = "11111111111111111111111111111111";
/// The marker digest recorded where no marker has those bytes.
pub(crate) const RECORDED_MARKER_DIGEST: &str = "0";

/// The case families of the finite D8/CF inventory, in the plan's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    /// Control and sticky batch.
    Control,
    /// Registry shape and caps.
    RegistryShape,
    /// References and intents.
    References,
    /// Receipts and markers.
    ReceiptsMarkers,
    /// Identity and volume.
    IdentityVolume,
    /// Checkout absence.
    Absence,
    /// Fence, declaration and policy.
    FenceDeclarationPolicy,
    /// Legacy and operator list.
    Legacy,
    /// Writers and custody (deferred).
    WritersCustody,
    /// Admission effects (deferred).
    AdmissionEffects,
}

impl Family {
    /// The family's name, as the D8/plan inventory prints it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Control => "control and sticky batch",
            Self::RegistryShape => "registry shape and caps",
            Self::References => "references and intents",
            Self::ReceiptsMarkers => "receipts and markers",
            Self::IdentityVolume => "identity and volume",
            Self::Absence => "checkout absence",
            Self::FenceDeclarationPolicy => "fence, declaration and policy",
            Self::Legacy => "legacy and operator list",
            Self::WritersCustody => "writers and custody (deferred, second batch)",
            Self::AdmissionEffects => "admission effects (deferred, second batch)",
        }
    }

    /// The CF applicability table with a reason for every CF.
    pub(crate) const fn cf(self) -> &'static [CfNote] {
        match self {
            Self::Control => CF_CONTROL,
            Self::RegistryShape => CF_REGISTRY,
            Self::References => CF_REFERENCES,
            Self::ReceiptsMarkers => CF_RECEIPTS,
            Self::IdentityVolume => CF_IDENTITY,
            Self::Absence => CF_ABSENCE,
            Self::FenceDeclarationPolicy => CF_FENCE,
            Self::Legacy => CF_LEGACY,
            Self::WritersCustody => CF_WRITERS,
            Self::AdmissionEffects => CF_ADMISSION,
        }
    }

    /// The decision sites this family's Barge run consumes.
    pub(crate) const fn obligations(self) -> &'static [Obligation] {
        match self {
            Self::Control => OBLIGATIONS_CONTROL,
            Self::RegistryShape => OBLIGATIONS_REGISTRY,
            Self::References => OBLIGATIONS_REFERENCES,
            Self::ReceiptsMarkers => OBLIGATIONS_RECEIPTS,
            Self::IdentityVolume => OBLIGATIONS_IDENTITY,
            Self::Absence => OBLIGATIONS_ABSENCE,
            Self::FenceDeclarationPolicy => OBLIGATIONS_FENCE,
            Self::Legacy => OBLIGATIONS_LEGACY,
            Self::WritersCustody | Self::AdmissionEffects => &[],
        }
    }

    /// The engine enumerations the fixture state triggers, with the caller.
    pub(crate) const fn enumerations(self) -> &'static [&'static str] {
        match self {
            Self::Control => &[
                "barge: area::list_names (plan roots)",
                "barge: cache::read_placements (placements)",
                "barge: area::read_declarations (declarations)",
                "barge: reference::read_receipts over read_dir(outputs)",
            ],
            Self::RegistryShape => &[],
            Self::References => &[
                "barge: state::read_intents_with_deadline over the intent journal",
                "barge: area::read_declarations (declarations)",
            ],
            Self::ReceiptsMarkers => &[
                "barge: reference::read_receipts over read_dir(outputs)",
                "barge: area::read_declarations (declarations)",
            ],
            Self::IdentityVolume => &[],
            Self::Absence => &[],
            Self::FenceDeclarationPolicy => &["barge: area::read_declarations (declarations)"],
            Self::Legacy => &[
                "barge: reference::read_receipts over read_dir(outputs)",
                "barge: area::read_declarations (declarations)",
            ],
            Self::WritersCustody | Self::AdmissionEffects => &[],
        }
    }
}

/// The seven counterfactual families the D8 matrix names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cf {
    /// An unbound extra folder in an enumerated set.
    Cf1,
    /// A writer between runs.
    Cf2,
    /// Identical bytes in a new inode.
    Cf3,
    /// A broken or exhausted tape.
    Cf4,
    /// An identity epoch change.
    Cf5,
    /// Missing admission inputs.
    Cf6,
    /// Second reads under changed epochs.
    Cf7,
}

impl Cf {
    /// The CF's name, as the plan and the contract print it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Cf1 => "CF-1",
            Self::Cf2 => "CF-2",
            Self::Cf3 => "CF-3",
            Self::Cf4 => "CF-4",
            Self::Cf5 => "CF-5",
            Self::Cf6 => "CF-6",
            Self::Cf7 => "CF-7",
        }
    }
}

/// One CF's applicability to a family, with the reason either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CfNote {
    /// The counterfactual.
    pub(crate) cf: Cf,
    /// Whether the family has its precondition.
    pub(crate) applies: bool,
    /// Why it applies or why it does not.
    pub(crate) reason: &'static str,
}

/// A premise an execution needs beyond this source preparation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Premise {
    /// An independently supplied export-closure digest and receipt.
    ExportClosure,
    /// A bound manifest whose digest the transcript records.
    BoundManifest,
    /// The recorder's in-window leaf bind, which the current recorder does not
    /// provide for a path-only file row.
    RecorderBind,
    /// Qualified K1: a held parent's lifetime across the required interval.
    K1,
    /// Writer closure or the declared absence of an unknown writer.
    WriterClosure,
    /// A second mount epoch inside or between windows.
    DynamicEpoch,
    /// A registry that fits the unchanged shared caps.
    SharedCaps,
    /// A real duplicate-UUID mount census.
    DuplicateMountCensus,
    /// An internal boot-Data placement.
    InternalPlacement,
    /// A recorded holder whose kernel start and uid were observed.
    HolderLifetime,
}

impl Premise {
    /// The premise's name, as the plan prints it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ExportClosure => "export-closure",
            Self::BoundManifest => "bound-manifest",
            Self::RecorderBind => "recorder-bind",
            Self::K1 => "k1-parent-lifetime",
            Self::WriterClosure => "writer-closure",
            Self::DynamicEpoch => "dynamic-epoch",
            Self::SharedCaps => "shared-caps",
            Self::DuplicateMountCensus => "duplicate-mount-census",
            Self::InternalPlacement => "internal-placement",
            Self::HolderLifetime => "holder-lifetime",
        }
    }
}

/// Whether the fixture state is prepared, and if not, why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Support {
    /// The deliberate state is written and observed by [`prepare`].
    Prepared,
    /// The fixture state needs evidence this preparation may not produce.
    Unsupported(&'static str),
    /// The family is deliberately held for the second batch.
    Deferred(&'static str),
}

/// One deliberate fixture path and the content it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlannedPath {
    /// Where the path is.
    pub(crate) slot: Slot,
    /// What is (or is deliberately not) there.
    pub(crate) content: Content,
}

/// A fixture location, resolved against the owned setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// Oxide's pool control directory.
    ControlDir,
    /// Oxide's registry document.
    Registry,
    /// Oxide's outputs directory, which Barge enumerates for receipts.
    OutputsDir,
    /// One receipt inside `outputs/`.
    Receipt,
    /// The owned root's Oxide marker.
    Marker,
    /// The pool's control state.
    ControlState,
    /// The pool's operator list.
    OperatorList,
    /// Barge's own intent journal directory.
    IntentJournal,
    /// One pending Barge intent document.
    Intent,
    /// A folder under the area that no registry row names.
    ExtraRoot,
    /// The checkout path the registry row names, which is absent.
    Checkout,
    /// The checkout's parent directory, present in the absence family.
    CheckoutAncestor,
    /// The checkout leaf below its parent, missing or replaced in the absence family.
    CheckoutLeaf,
}

impl Slot {
    /// The declared relative shape, as the plan prints it. The placeholders are
    /// never resolved by the plan.
    pub(crate) const fn shape(self) -> &'static str {
        match self {
            Self::ControlDir => "<pool>/oxide-managed/",
            Self::Registry => "<pool>/oxide-managed/outputs/registry.json",
            Self::OutputsDir => "<pool>/oxide-managed/outputs/",
            Self::Receipt => "<pool>/oxide-managed/outputs/<checkout-id>.json",
            Self::Marker => "<mount>/oxide-outputs/<root>/.oxide-output.json",
            Self::ControlState => "<pool>/oxide-managed/state.json",
            Self::OperatorList => "<pool>/operator-list.json",
            Self::IntentJournal => "<fixture>/state/intents/",
            Self::Intent => "<fixture>/state/intents/<intent-id>.json",
            Self::ExtraRoot => "<mount>/oxide-outputs/<extra-root>/",
            Self::Checkout => "<fixture>/checkout",
            Self::CheckoutAncestor => "<fixture>/checkout/",
            Self::CheckoutLeaf => "<fixture>/checkout/target",
        }
    }
}

/// The document a planned path holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Content {
    /// A real directory, created empty.
    Directory,
    /// The path is deliberately absent.
    Absent,
    /// Oxide's registry, in one reviewed shape.
    Registry(RegistryShape),
    /// One Oxide receipt, in one reviewed shape.
    Receipt(ReceiptShape),
    /// The owned root's Oxide marker, in one reviewed shape.
    Marker(MarkerShape),
    /// The pool's control state.
    Control(ControlShape),
    /// Oxide's operator list: a JSON array of absolute root paths.
    OperatorList,
    /// One pending `barge.intent/2` record naming the owned root.
    BargeIntent,
}

/// The registry documents the family inventory needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistryShape {
    /// One settled row naming the owned root.
    Base,
    /// `complete: false` with the record of why.
    Incomplete,
    /// Bytes that are not the document Barge reads.
    Malformed,
    /// A document larger than the registry's read cap.
    Oversize,
    /// Two live rows naming one root from two checkouts.
    Contradictory,
    /// Sticky incompleteness carrying the cancelled-overflow tag.
    Overflow,
    /// The row is pending.
    Pending,
    /// The row carries a reservation.
    Reservation,
    /// The row is cancelling.
    Cancelling,
    /// The row is pruning, with an unfinished journal entry.
    Pruning,
    /// The row is selected while its receipt selects nothing.
    SelectedNotCurrent,
    /// No rows at all: the marked folder is outside the registry.
    Empty,
    /// The recorded nonce differs from the marker's.
    StaleNonce,
    /// The recorded generation differs from the marker's.
    StaleGeneration,
    /// The recorded root inode differs from the owned root's.
    StaleInode,
    /// The recorded root device differs from the owned root's.
    StaleDevice,
    /// The recorded marker digest is not the marker's digest.
    ReplacedMarker,
}

/// The receipt documents the family inventory needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptShape {
    /// A committed `/2` receipt selecting the owned root.
    Base,
    /// Bytes that are not the document Barge reads.
    Malformed,
    /// A `/2` receipt in a non-committed state.
    NonCommitted,
    /// A committed receipt naming another checkout.
    Contradictory,
    /// A committed `/2` receipt that selects nothing.
    NonSelecting,
    /// A committed `/1` whole-target receipt.
    V1,
}

/// The Oxide marker documents the family inventory needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkerShape {
    /// A valid `/2` marker naming the owned root and [`NONCE`].
    Base,
    /// Bytes that are not the document Barge reads.
    Malformed,
    /// A `/1` marker, which Barge reports as unknown.
    V1,
    /// A valid `/2` marker recording [`OTHER_NONCE`].
    OtherNonce,
    /// A valid `/2` marker whose bytes no recorded digest names.
    Replaced,
}

/// The pool control-state shapes the fence family needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlShape {
    /// A version-9 state carrying every fence, with no records.
    Complete,
    /// A version-9 state whose owned-outputs fence is unknown.
    UnknownFence,
}

/// The decision-site identity slot an obligation declares. The slot is a kind,
/// never an identity: only an execution can bind it to an observed leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotKind {
    /// A path plus its parent and own device/inode.
    FileLike,
    /// A component plus its parent's device/inode.
    Component,
    /// A process: pid, start and uid.
    Process,
    /// A volume: uuid, mount, device and window identity.
    Volume,
}

impl SlotKind {
    /// The slot's name, as the plan prints it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::FileLike => "file-like",
            Self::Component => "component",
            Self::Process => "process",
            Self::Volume => "volume",
        }
    }
}

/// One decision site a case's engine run consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Obligation {
    /// The engine whose recorder emits the row.
    pub(crate) engine: &'static str,
    /// The emitted site, as `<source symbol>#<observation family>`.
    pub(crate) site: &'static str,
    /// The identity alternative the obligation needs.
    pub(crate) slot: SlotKind,
    /// Whether the site's parent must be proven.
    pub(crate) parent_required: bool,
}

/// One case specification: data only, never bytes or identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaseSpec {
    /// The D8 case id.
    pub(crate) id: &'static str,
    /// The family it belongs to.
    pub(crate) family: Family,
    /// Whether its fixture state is prepared here.
    pub(crate) support: Support,
    /// The deliberate state this preparation writes.
    pub(crate) files: &'static [PlannedPath],
    /// The in-window actions this preparation does not perform.
    pub(crate) window_actions: &'static [&'static str],
    /// The premises an execution still needs.
    pub(crate) premises: &'static [Premise],
    /// The decision sites the case's Barge run consumes.
    pub(crate) obligations: &'static [Obligation],
    /// The manifest's single declared classification.
    pub(crate) classification: &'static str,
    /// The D8 relationship, verbatim, so a declared difference is never hidden
    /// behind the single classification field.
    pub(crate) relationship: &'static str,
    /// The declared fates, by root slot (`owned` or `extra`).
    pub(crate) expected_roots: &'static [(&'static str, &'static str)],
    /// CF applicability with a reason for every CF.
    pub(crate) cf: &'static [CfNote],
}

// --- CF tables -----------------------------------------------------------

const fn cf_note(cf: Cf, applies: bool, reason: &'static str) -> CfNote {
    CfNote {
        cf,
        applies,
        reason,
    }
}

const CF_CONTROL: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        true,
        "Barge enumerates plan roots, placements, declarations and Oxide receipts on every run",
    ),
    cf_note(
        Cf::Cf2,
        false,
        "the fixture starts no writer; custody is the deferred second batch",
    ),
    cf_note(
        Cf::Cf3,
        false,
        "this family replaces no marker in the window",
    ),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(
        Cf::Cf5,
        false,
        "no identity epoch changes inside the window",
    ),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(
        Cf::Cf7,
        false,
        "no second read happens under a changed epoch",
    ),
];

const CF_REGISTRY: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        false,
        "the fixture creates no enumerated set; the case decides on the registry file itself",
    ),
    cf_note(Cf::Cf2, false, "no writer participates"),
    cf_note(Cf::Cf3, false, "no root marker exists in this family"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(Cf::Cf7, false, "no second read under a changed epoch"),
];

const CF_REFERENCES: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        true,
        "Barge enumerates the intent journal and the declarations; the registry document's own journal is a projection, not a directory read",
    ),
    cf_note(Cf::Cf2, false, "no writer participates"),
    cf_note(Cf::Cf3, false, "no root marker is replaced"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(Cf::Cf7, false, "no second read under a changed epoch"),
];

const CF_RECEIPTS: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        true,
        "Barge enumerates the outputs directory for receipts, and the declarations",
    ),
    cf_note(Cf::Cf2, false, "no writer participates"),
    cf_note(
        Cf::Cf3,
        true,
        "the replaced-marker case rewrites identical bytes into a new inode in the window",
    ),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(
        Cf::Cf7,
        false,
        "no second read under a changed epoch inside this fixture",
    ),
];

const CF_IDENTITY: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        false,
        "the fixture enumerates no receipt, intent, declaration or placement set for these cases",
    ),
    cf_note(
        Cf::Cf2,
        false,
        "only the dynamic rows start a writer, and those cases are unsupported here",
    ),
    cf_note(
        Cf::Cf3,
        true,
        "the marker and root identity families read a root marker",
    ),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(
        Cf::Cf5,
        false,
        "the dynamic rows are the only epoch changes, and they are unsupported here",
    ),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(
        Cf::Cf7,
        true,
        "the identity family reads the marker again under the second root-id lock",
    ),
];

const CF_ABSENCE: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        false,
        "no receipt, intent, declaration or placement set is enumerated",
    ),
    cf_note(Cf::Cf2, false, "no writer participates"),
    cf_note(Cf::Cf3, false, "no root marker is replaced"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(
        Cf::Cf7,
        true,
        "the absence walk revalidates its chain under a second epoch",
    ),
];

const CF_FENCE: &[CfNote] = &[
    cf_note(Cf::Cf1, true, "Barge enumerates the area declarations"),
    cf_note(Cf::Cf2, false, "no writer participates"),
    cf_note(Cf::Cf3, false, "no root marker is replaced"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(
        Cf::Cf6,
        false,
        "no admission input is missing; the admission family is deferred",
    ),
    cf_note(Cf::Cf7, false, "no second read under a changed epoch"),
];

const CF_LEGACY: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        true,
        "Barge enumerates the outputs directory for receipts and the declarations",
    ),
    cf_note(Cf::Cf2, false, "no writer participates"),
    cf_note(Cf::Cf3, false, "no root marker is replaced"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(Cf::Cf7, false, "no second read under a changed epoch"),
];

const CF_WRITERS: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        false,
        "no receipt, intent, declaration or placement set is enumerated",
    ),
    cf_note(
        Cf::Cf2,
        true,
        "the fixture would start the declared holder for window A only",
    ),
    cf_note(Cf::Cf3, false, "no root marker is replaced"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(
        Cf::Cf5,
        true,
        "the custody family observes an identity epoch change",
    ),
    cf_note(Cf::Cf6, false, "no admission input is missing"),
    cf_note(
        Cf::Cf7,
        false,
        "no second read under a changed epoch inside this fixture",
    ),
];

const CF_ADMISSION: &[CfNote] = &[
    cf_note(
        Cf::Cf1,
        false,
        "no receipt, intent, declaration or placement set is enumerated",
    ),
    cf_note(
        Cf::Cf2,
        true,
        "the case plants an admission input a writer owns",
    ),
    cf_note(Cf::Cf3, false, "no root marker is replaced"),
    cf_note(
        Cf::Cf4,
        true,
        "every family carries the broken or exhausted tape check",
    ),
    cf_note(Cf::Cf5, false, "no identity epoch changes"),
    cf_note(
        Cf::Cf6,
        true,
        "the case's whole point is a missing admission input",
    ),
    cf_note(Cf::Cf7, false, "no second read under a changed epoch"),
];

// --- decision sites ------------------------------------------------------

const fn site(site: &'static str, slot: SlotKind, parent_required: bool) -> Obligation {
    Obligation {
        engine: "barge",
        site,
        slot,
        parent_required,
    }
}

const OBLIGATIONS_CONTROL: &[Obligation] = &[
    site("reference::read_registry#file", SlotKind::FileLike, false),
    site("state::read_bounded_inner#stat", SlotKind::FileLike, false),
    site("area::list_names#dir-list", SlotKind::FileLike, false),
    site(
        "reference::read_receipts_with#file",
        SlotKind::FileLike,
        false,
    ),
    site("area::read_directory#dir-list", SlotKind::FileLike, false),
    site("reference::pools_holding#stat", SlotKind::FileLike, false),
];

const OBLIGATIONS_REGISTRY: &[Obligation] = &[
    site("reference::read_registry#file", SlotKind::FileLike, false),
    site("state::read_bounded_inner#stat", SlotKind::FileLike, false),
];

const OBLIGATIONS_REFERENCES: &[Obligation] = &[
    site("reference::read_registry#file", SlotKind::FileLike, false),
    site(
        "state::read_intents_with_deadline#file",
        SlotKind::FileLike,
        false,
    ),
    site("area::read_directory#dir-list", SlotKind::FileLike, false),
];

const OBLIGATIONS_RECEIPTS: &[Obligation] = &[
    site(
        "reference::read_receipts_with::precheck#stat",
        SlotKind::FileLike,
        false,
    ),
    site(
        "reference::read_receipts_with#file",
        SlotKind::FileLike,
        false,
    ),
    site(
        "reference::read_oxide_marker#stat",
        SlotKind::Component,
        true,
    ),
    site(
        "reference::read_oxide_marker#file",
        SlotKind::Component,
        true,
    ),
];

const OBLIGATIONS_IDENTITY: &[Obligation] = &[
    site("reference::read_registry#file", SlotKind::FileLike, false),
    site(
        "reference::read_oxide_marker#stat",
        SlotKind::Component,
        true,
    ),
    site(
        "area::VolumeRoot::open_with#volume",
        SlotKind::Volume,
        false,
    ),
    site("area::resolve_by#dir-list", SlotKind::FileLike, false),
];

const OBLIGATIONS_ABSENCE: &[Obligation] = &[
    site("MacOs::child_facts#stat", SlotKind::Component, true),
    site("MacOs::read_child_file#file", SlotKind::Component, true),
    site("MacOs::open_child_dir#stat", SlotKind::Component, true),
];

const OBLIGATIONS_FENCE: &[Obligation] = &[
    site("reference::read_registry#file", SlotKind::FileLike, false),
    site(
        "pool_custody::read_document#file",
        SlotKind::FileLike,
        false,
    ),
    site("area::read_directory#dir-list", SlotKind::FileLike, false),
];

const OBLIGATIONS_LEGACY: &[Obligation] = &[
    site("reference::read_registry#file", SlotKind::FileLike, false),
    site(
        "reference::read_receipts_with#file",
        SlotKind::FileLike,
        false,
    ),
    site(
        "reference::read_oxide_marker#stat",
        SlotKind::Component,
        true,
    ),
];

/// The premises every prepared first-batch case still needs.
const PREPARED_PREMISES: &[Premise] = &[
    Premise::ExportClosure,
    Premise::BoundManifest,
    Premise::RecorderBind,
];

/// The premises one unsupported case still needs, named per blocker.
const PREMISE_DYNAMIC_EPOCH: &[Premise] = &[Premise::DynamicEpoch];
const PREMISE_SHARED_CAPS: &[Premise] = &[Premise::SharedCaps];
const PREMISE_DUPLICATE_CENSUS: &[Premise] = &[Premise::DuplicateMountCensus];
const PREMISE_INTERNAL_PLACEMENT: &[Premise] = &[Premise::InternalPlacement];
const PREMISE_DEFERRED: &[Premise] = &[Premise::WriterClosure, Premise::HolderLifetime];

/// The premises a marker- or absence-reading case adds: K1 parent lifetime.
const K1_PREMISES: &[Premise] = &[
    Premise::ExportClosure,
    Premise::BoundManifest,
    Premise::RecorderBind,
    Premise::K1,
];

// --- the finite case table ----------------------------------------------

const fn planned(slot: Slot, content: Content) -> PlannedPath {
    PlannedPath { slot, content }
}

const NO_WINDOW_ACTIONS: &[&str] = &[];

const fn prepared(
    id: &'static str,
    family: Family,
    files: &'static [PlannedPath],
    window_actions: &'static [&'static str],
    classification: &'static str,
    relationship: &'static str,
    expected_roots: &'static [(&'static str, &'static str)],
) -> CaseSpec {
    CaseSpec {
        id,
        family,
        support: Support::Prepared,
        files,
        window_actions,
        premises: PREPARED_PREMISES,
        obligations: family.obligations(),
        classification,
        relationship,
        expected_roots,
        cf: family.cf(),
    }
}

const fn prepared_k1(
    id: &'static str,
    family: Family,
    files: &'static [PlannedPath],
    window_actions: &'static [&'static str],
    classification: &'static str,
    relationship: &'static str,
    expected_roots: &'static [(&'static str, &'static str)],
) -> CaseSpec {
    CaseSpec {
        id,
        family,
        support: Support::Prepared,
        files,
        window_actions,
        premises: K1_PREMISES,
        obligations: family.obligations(),
        classification,
        relationship,
        expected_roots,
        cf: family.cf(),
    }
}

const fn unsupported(
    id: &'static str,
    family: Family,
    reason: &'static str,
    premises: &'static [Premise],
    relationship: &'static str,
) -> CaseSpec {
    CaseSpec {
        id,
        family,
        support: Support::Unsupported(reason),
        files: &[],
        window_actions: NO_WINDOW_ACTIONS,
        premises,
        obligations: family.obligations(),
        classification: "SCOPE",
        relationship,
        expected_roots: &[],
        cf: family.cf(),
    }
}

const fn deferred(id: &'static str, family: Family, reason: &'static str) -> CaseSpec {
    CaseSpec {
        id,
        family,
        support: Support::Deferred(reason),
        files: &[],
        window_actions: NO_WINDOW_ACTIONS,
        premises: PREMISE_DEFERRED,
        obligations: &[],
        classification: "SCOPE",
        relationship: "deferred: the second batch is held",
        expected_roots: &[],
        cf: family.cf(),
    }
}

/// The BASE-shaped state: the outputs directory and the settled registry row.
const BASE_STATE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Checkout, Content::Absent),
];

const REG_INCOMPLETE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Incomplete)),
];
const REG_MALFORMED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Malformed)),
];
const REG_OVERSIZE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Oversize)),
];
const REG_CONTRADICTORY: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(
        Slot::Registry,
        Content::Registry(RegistryShape::Contradictory),
    ),
];
const REG_OVERFLOW: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Overflow)),
];
const REG_MISSING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Absent),
];
const REF_PENDING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Pending)),
    planned(Slot::IntentJournal, Content::Directory),
];
const REF_RESERVATION: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(
        Slot::Registry,
        Content::Registry(RegistryShape::Reservation),
    ),
    planned(Slot::IntentJournal, Content::Directory),
];
const REF_CANCELLING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Cancelling)),
    planned(Slot::IntentJournal, Content::Directory),
];
const REF_PRUNING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Pruning)),
    planned(Slot::IntentJournal, Content::Directory),
    planned(Slot::Intent, Content::BargeIntent),
];
const REF_SELECTED_NOT_CURRENT: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(
        Slot::Registry,
        Content::Registry(RegistryShape::SelectedNotCurrent),
    ),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::NonSelecting)),
    planned(Slot::IntentJournal, Content::Directory),
];
const REF_ORPHAN_MARKED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Empty)),
    planned(Slot::ExtraRoot, Content::Directory),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
    planned(Slot::IntentJournal, Content::Directory),
];
const RCP_MISSING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Absent),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const RCP_MALFORMED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::Malformed)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const RCP_NONCOMMITTED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::NonCommitted)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const RCP_CONTRADICTORY: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::Contradictory)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const RCP_V1: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::V1)),
    planned(Slot::Marker, Content::Marker(MarkerShape::V1)),
];
const MRK_MISSING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::Base)),
    planned(Slot::Marker, Content::Absent),
];
const MRK_MALFORMED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Malformed)),
];
const MRK_REPLACED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(
        Slot::Registry,
        Content::Registry(RegistryShape::ReplacedMarker),
    ),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Replaced)),
];
const ID_NONCE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::StaleNonce)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const ID_GENERATION: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(
        Slot::Registry,
        Content::Registry(RegistryShape::StaleGeneration),
    ),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const ID_INODE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::StaleInode)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const ID_DEVICE_STALE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(
        Slot::Registry,
        Content::Registry(RegistryShape::StaleDevice),
    ),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const VOL_DUP_REGISTRY: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
];
const ABS_UUID_CONFLICT: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::OtherNonce)),
    planned(Slot::Checkout, Content::Absent),
];
const ABS_ANCESTOR_MISSING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::CheckoutAncestor, Content::Directory),
    planned(Slot::CheckoutLeaf, Content::Absent),
];
const ABS_ANCESTOR_REPLACED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::CheckoutAncestor, Content::Directory),
    planned(Slot::CheckoutLeaf, Content::Directory),
];
const FENCE_MISSING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::ControlState, Content::Absent),
];
const FENCE_UNKNOWN: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(
        Slot::ControlState,
        Content::Control(ControlShape::UnknownFence),
    ),
];
const DECL_MISSING: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
];
const DECL_POLICY_NONE: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
];
const LEG_UNLISTED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
    planned(Slot::OperatorList, Content::Absent),
    planned(Slot::Checkout, Content::Absent),
];
const LEG_LISTED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
    planned(Slot::OperatorList, Content::OperatorList),
    planned(Slot::Checkout, Content::Absent),
];
const LEG_LISTED_MARKED: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Receipt, Content::Receipt(ReceiptShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
    planned(Slot::OperatorList, Content::OperatorList),
    planned(Slot::Checkout, Content::Absent),
];
const LEG_RECEIPTLESS: &[PlannedPath] = &[
    planned(Slot::ControlDir, Content::Directory),
    planned(Slot::OutputsDir, Content::Directory),
    planned(Slot::Registry, Content::Registry(RegistryShape::Base)),
    planned(Slot::Marker, Content::Marker(MarkerShape::Base)),
    planned(Slot::OperatorList, Content::OperatorList),
    planned(Slot::Receipt, Content::Absent),
    planned(Slot::Checkout, Content::Absent),
];

const RELEASED: &[(&str, &str)] = &[("owned", "release")];
const KEPT: &[(&str, &str)] = &[("owned", "keep")];
const KEPT_AND_CANDIDATE: &[(&str, &str)] = &[("owned", "keep"), ("extra", "release")];

/// Every case of the finite D8/CF inventory, in the plan's order.
pub(crate) const CASES: &[CaseSpec] = &[
    prepared(
        "CTL-0",
        Family::Control,
        BASE_STATE,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH, effects empty",
        RELEASED,
    ),
    prepared(
        "CTL-1",
        Family::Control,
        REG_INCOMPLETE,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH-KEPT",
        KEPT,
    ),
    unsupported(
        "CAP-1",
        Family::RegistryShape,
        "CAP-1 needs more than 8192 representative registry rows, which exceeds the unchanged shared \
         caps; an overflowed or truncated document is insufficient by rule, so no fixture is prepared",
        PREMISE_SHARED_CAPS,
        "engine-local cap test, no shared parity",
    ),
    prepared(
        "REG-MISSING",
        Family::RegistryShape,
        REG_MISSING,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared(
        "REG-MALFORMED",
        Family::RegistryShape,
        REG_MALFORMED,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH-KEPT",
        KEPT,
    ),
    prepared(
        "REG-OVERSIZE",
        Family::RegistryShape,
        REG_OVERSIZE,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH-KEPT",
        KEPT,
    ),
    prepared(
        "REG-CONTRADICTORY",
        Family::RegistryShape,
        REG_CONTRADICTORY,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared(
        "REG-OVERFLOW",
        Family::RegistryShape,
        REG_OVERFLOW,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared(
        "REF-PENDING",
        Family::References,
        REF_PENDING,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH + SCOPE",
        KEPT,
    ),
    prepared(
        "REF-RESERVATION",
        Family::References,
        REF_RESERVATION,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared(
        "REF-CANCELLING",
        Family::References,
        REF_CANCELLING,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared(
        "REF-PRUNING",
        Family::References,
        REF_PRUNING,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE (whole-command vs named-root)",
        KEPT,
    ),
    prepared(
        "REF-SELECTED-NOT-CURRENT",
        Family::References,
        REF_SELECTED_NOT_CURRENT,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared_k1(
        "REF-ORPHAN-MARKED",
        Family::References,
        REF_ORPHAN_MARKED,
        NO_WINDOW_ACTIONS,
        "EXPLAIN-OR-BLOCK",
        "EXPLAIN-OR-BLOCK",
        KEPT_AND_CANDIDATE,
    ),
    prepared(
        "RCP-MISSING",
        Family::ReceiptsMarkers,
        RCP_MISSING,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared(
        "RCP-MALFORMED",
        Family::ReceiptsMarkers,
        RCP_MALFORMED,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared(
        "RCP-NONCOMMITTED",
        Family::ReceiptsMarkers,
        RCP_NONCOMMITTED,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared(
        "RCP-CONTRADICTORY",
        Family::ReceiptsMarkers,
        RCP_CONTRADICTORY,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared_k1(
        "RCP-V1",
        Family::ReceiptsMarkers,
        RCP_V1,
        NO_WINDOW_ACTIONS,
        "EXPLAIN-OR-BLOCK",
        "EXPLAIN-OR-BLOCK",
        KEPT,
    ),
    prepared_k1(
        "MRK-MISSING",
        Family::ReceiptsMarkers,
        MRK_MISSING,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "MRK-MALFORMED",
        Family::ReceiptsMarkers,
        MRK_MALFORMED,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "MRK-REPLACED",
        Family::ReceiptsMarkers,
        MRK_REPLACED,
        &[
            "rewrite the marker's identical bytes into a new inode after the pre-capture (CF-3); this \
             preparation writes only the pre-window state",
        ],
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "ID-NONCE",
        Family::ReceiptsMarkers,
        ID_NONCE,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared_k1(
        "ID-GENERATION",
        Family::ReceiptsMarkers,
        ID_GENERATION,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    prepared_k1(
        "ID-INODE",
        Family::IdentityVolume,
        ID_INODE,
        NO_WINDOW_ACTIONS,
        "D-2",
        "D-2 (batch versus root; the second eligible root is the case's own state)",
        KEPT,
    ),
    prepared_k1(
        "ID-DEVICE-STALE",
        Family::IdentityVolume,
        ID_DEVICE_STALE,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH (current binding decides, never the stale prose)",
        RELEASED,
    ),
    unsupported(
        "ID-DEVICE-LIVE",
        Family::IdentityVolume,
        "a mid-window remount needs a second mount epoch and CF-2/CF-5 writers; this preparation may \
         not mount or remount anything, so the case is not prepared",
        PREMISE_DYNAMIC_EPOCH,
        "D-1 (declared dynamic)",
    ),
    unsupported(
        "VOL-ABSENT",
        Family::IdentityVolume,
        "the fixture volume would have to be positively absent while the fixture itself lives on a \
         volume; that needs a real detach, which this preparation may not perform",
        PREMISE_DYNAMIC_EPOCH,
        "MATCH-KEPT + SCOPE",
    ),
    unsupported(
        "VOL-REMOVED-DURING",
        Family::IdentityVolume,
        "a detach between windows needs two mount epochs and its own per-window mount census; this \
         preparation may not detach anything",
        PREMISE_DYNAMIC_EPOCH,
        "declared dynamic",
    ),
    prepared_k1(
        "VOL-DUP-REGISTRY",
        Family::IdentityVolume,
        VOL_DUP_REGISTRY,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "declared per-engine document record, no parity claim",
        KEPT,
    ),
    unsupported(
        "VOL-DUP-LIVE",
        Family::IdentityVolume,
        "a real duplicate-UUID mount census cannot be produced from synthetic fixtures without kernel \
         forgery; declared coverage limit",
        PREMISE_DUPLICATE_CENSUS,
        "coverage limit, not parity",
    ),
    prepared_k1(
        "ABS-UUID-CONFLICT",
        Family::IdentityVolume,
        ABS_UUID_CONFLICT,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "ABS-ANCESTOR-MISSING",
        Family::Absence,
        ABS_ANCESTOR_MISSING,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "ABS-ANCESTOR-REPLACED",
        Family::Absence,
        ABS_ANCESTOR_REPLACED,
        &[
            "replace the ancestor's directory with a new inode after the pre-capture (CF-7); this \
             preparation writes only the pre-window state",
        ],
        "MATCH",
        "MATCH",
        KEPT,
    ),
    unsupported(
        "ABS-BOOT-DATA",
        Family::Absence,
        "the case needs an internal boot-Data placement, which this preparation may not create or touch",
        PREMISE_INTERNAL_PLACEMENT,
        "D-3-scope",
    ),
    prepared(
        "FENCE-MISSING",
        Family::FenceDeclarationPolicy,
        FENCE_MISSING,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH-KEPT",
        KEPT,
    ),
    prepared(
        "FENCE-UNKNOWN",
        Family::FenceDeclarationPolicy,
        FENCE_UNKNOWN,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared(
        "DECL-MISSING",
        Family::FenceDeclarationPolicy,
        DECL_MISSING,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "SCOPE (Barge refuses the command; Oxide is unaffected)",
        KEPT,
    ),
    prepared(
        "DECL-POLICY-NONE",
        Family::FenceDeclarationPolicy,
        DECL_POLICY_NONE,
        NO_WINDOW_ACTIONS,
        "CONFIG",
        "CONFIG (no parity)",
        KEPT,
    ),
    prepared_k1(
        "LEG-UNLISTED",
        Family::Legacy,
        LEG_UNLISTED,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "LEG-LISTED",
        Family::Legacy,
        LEG_LISTED,
        NO_WINDOW_ACTIONS,
        "D-3",
        "D-3 (declared)",
        KEPT,
    ),
    prepared_k1(
        "LEG-LISTED-MARKED",
        Family::Legacy,
        LEG_LISTED_MARKED,
        NO_WINDOW_ACTIONS,
        "MATCH",
        "MATCH",
        KEPT,
    ),
    prepared_k1(
        "LEG-RECEIPTLESS",
        Family::Legacy,
        LEG_RECEIPTLESS,
        NO_WINDOW_ACTIONS,
        "SCOPE",
        "MATCH-KEPT + SCOPE",
        KEPT,
    ),
    deferred(
        "WRT-LIVE-THIS",
        Family::WritersCustody,
        "the second batch is deliberately held: recorded holders carry a null kernel start, so the \
         custody decision would rest on an unproven holder",
    ),
    deferred(
        "WRT-LIVE-OTHER",
        Family::WritersCustody,
        "the second batch is deliberately held: it needs a removable sibling pool and a live holder",
    ),
    deferred(
        "WRT-UNKNOWN",
        Family::WritersCustody,
        "the second batch is deliberately held: a lost writer and its unclosed fact are custody \
         evidence this preparation may not fabricate",
    ),
    deferred(
        "WRT-ACQUIRING",
        Family::WritersCustody,
        "the second batch is deliberately held: it starts a spawning holder",
    ),
    deferred(
        "WRT-RETIRED-CLEAN",
        Family::WritersCustody,
        "the second batch is deliberately held: EXPLAIN-OR-BLOCK is not available from current source",
    ),
    deferred(
        "EFFECT-1",
        Family::AdmissionEffects,
        "the second batch is deliberately held: it drives an admission-lock create, so an attempted \
         or partial effect is in scope",
    ),
    deferred(
        "EFFECT-2",
        Family::AdmissionEffects,
        "the second batch is deliberately held: it drives a fence save, so a completed effect is in \
         scope",
    ),
];

/// Look one case up by id. An unlisted id is refused; there is no BASE fallback.
pub(crate) fn spec(id: &str) -> Result<&'static CaseSpec, String> {
    CASES
        .iter()
        .find(|case| case.id == id)
        .ok_or_else(|| format!("`{id}` is not a case of the limited first-batch inventory"))
}

// --- the effect-free plan ------------------------------------------------

/// The effect-free plan for every case: what would be prepared, what each case
/// obliges, and exactly which cases are unsupported or deferred and why. It
/// creates nothing, opens no window and evaluates nothing.
pub(crate) fn plan_document() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = CASES
        .iter()
        .map(|case| {
            let (status, fixture) = match case.support {
                Support::Prepared => ("insufficient", "prepared"),
                Support::Unsupported(reason) => ("insufficient", reason),
                Support::Deferred(_) => ("deferred", "not prepared"),
            };
            let cf: Vec<serde_json::Value> = case
                .cf
                .iter()
                .map(|note| {
                    serde_json::json!({
                        "cf": note.cf.name(),
                        "applies": note.applies,
                        "reason": note.reason,
                    })
                })
                .collect();
            let obligations: Vec<serde_json::Value> = case
                .obligations
                .iter()
                .map(|obligation| {
                    serde_json::json!({
                        "engine": obligation.engine,
                        "site": obligation.site,
                        "identity_slot": obligation.slot.name(),
                        "parent_required": obligation.parent_required,
                    })
                })
                .collect();
            serde_json::json!({
                "id": case.id,
                "family": case.family.name(),
                "status": status,
                "fixture": fixture,
                "classification": case.classification,
                "relationship": case.relationship,
                "files": case.files.iter().map(|file| file.slot.shape()).collect::<Vec<_>>(),
                "window_actions": case.window_actions,
                "premises": case.premises.iter().map(|premise| premise.name()).collect::<Vec<_>>(),
                "enumerations": case.family.enumerations(),
                "obligations": obligations,
                "cf": cf,
            })
        })
        .collect();
    let deferred = CASES
        .iter()
        .filter(|case| matches!(case.support, Support::Deferred(_)))
        .count();
    let unsupported = CASES
        .iter()
        .filter(|case| matches!(case.support, Support::Unsupported(_)))
        .count();
    serde_json::json!({
        "document": PLAN_DOCUMENT,
        "runnable": 0,
        "counts": {
            "total": CASES.len(),
            "prepared": CASES.len() - deferred - unsupported,
            "unsupported": unsupported,
            "deferred": deferred,
        },
        "note": "Effect-free preparation only: no fixture exists, no window opens, no engine evaluates \
                 and no comparison credit follows.",
        "cases": rows,
    })
}

// --- owned preparation ---------------------------------------------------

/// The fixture's concrete locations for one prepared case.
struct Layout {
    dir: PathBuf,
    mount: PathBuf,
    uuid: VolumeUuid,
    pool: PathBuf,
    outputs: PathBuf,
    root: Option<PathBuf>,
    extra_root: PathBuf,
    checkout: PathBuf,
}

impl Layout {
    fn of(setup: &FixtureSetup) -> Self {
        let pool = setup.account.tool_cache.join("build-pool");
        let outputs = pool.join(OXIDE_CONTROL_DIR).join(OXIDE_OUTPUTS_DIR);
        Self {
            dir: setup.dir.path().to_path_buf(),
            mount: setup.mount.clone(),
            uuid: setup.record.volume_uuid.clone(),
            pool,
            outputs,
            root: setup.root.as_ref().map(|root| root.path.clone()),
            extra_root: setup.volume_root.join(OXIDE_OUTPUTS_DIR).join("extra-root"),
            checkout: setup.account.home.join("checkout"),
        }
    }

    fn resolve(&self, slot: Slot) -> PathBuf {
        match slot {
            Slot::ControlDir => self.pool.join(CONTROL_DIR),
            Slot::Registry => self.outputs.join(OXIDE_REGISTRY_FILE),
            Slot::OutputsDir => self.outputs.clone(),
            Slot::Receipt => self.outputs.join(format!("{CHECKOUT_ID}.json")),
            Slot::Marker => match &self.root {
                Some(root) => root.join(OXIDE_MARKER_FILE),
                None => self.dir.join("no-root").join(OXIDE_MARKER_FILE),
            },
            Slot::ControlState => self.pool.join(CONTROL_DIR).join(STATE_FILE),
            Slot::OperatorList => self.pool.join("operator-list.json"),
            Slot::IntentJournal => self.dir.join("state").join("intents"),
            Slot::Intent => self
                .dir
                .join("state")
                .join("intents")
                .join(format!("{INTENT_ID}.json")),
            Slot::ExtraRoot => self.extra_root.clone(),
            Slot::Checkout | Slot::CheckoutAncestor => self.checkout.clone(),
            Slot::CheckoutLeaf => self.checkout.join("target"),
        }
    }
}

/// One declared path's actual observed facts, as the manifest records them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeclaredEntry {
    /// The declared relative shape.
    pub(crate) shape: &'static str,
    /// The path inside the fixture, relative to the fixture root.
    pub(crate) rel: String,
    /// `file`, `directory` or `absent`.
    pub(crate) kind: &'static str,
    /// The path's own device, when it exists.
    pub(crate) device: Option<u64>,
    /// The path's own inode, when it exists.
    pub(crate) inode: Option<u64>,
    /// The path's own mode, when it exists.
    pub(crate) mode: Option<u32>,
    /// The path's size, when it exists.
    pub(crate) size: Option<u64>,
    /// The bytes digest, for a file.
    pub(crate) bytes_sha256: Option<String>,
    /// The parent directory's own device.
    pub(crate) parent_device: u64,
    /// The parent directory's own inode.
    pub(crate) parent_inode: u64,
}

/// One prepared case: the owned fixture, its declared and observed state, and
/// the case manifest. Nothing here ran.
#[derive(Debug)]
pub(crate) struct PreparedCase {
    /// The fixture directory and the manifest bytes.
    pub(crate) fixture: crate::tape_harness::EmbeddedFixture,
    /// The case that was prepared.
    pub(crate) spec: &'static CaseSpec,
    /// Every declared path's observed facts, in declaration order.
    pub(crate) entries: Vec<DeclaredEntry>,
    /// The owned roots the case declares.
    pub(crate) roots: Vec<String>,
}

/// Materialize one prepared case: write only its declared state inside a fresh
/// owned fixture, observe what was actually written, and encode the case
/// manifest. Unsupported, deferred and unlisted cases are refused by name, so a
/// non-BASE case can never be silently served the BASE state.
pub(crate) fn prepare(label: &str, id: &str) -> Result<PreparedCase, String> {
    prepare_in(&crate::testkit::scratch_root(), label, id)
}

/// [`prepare`] inside a run root the caller owns, so a caller can prove a
/// refused case created nothing.
pub(crate) fn prepare_in(
    root: &crate::testkit::RunRoot,
    label: &str,
    id: &str,
) -> Result<PreparedCase, String> {
    let case = spec(id)?;
    match case.support {
        Support::Prepared => {}
        Support::Unsupported(reason) => {
            return Err(format!("case {id} is unsupported: {reason}"));
        }
        Support::Deferred(reason) => {
            return Err(format!("case {id} is deferred: {reason}"));
        }
    }
    let setup =
        crate::tape_harness::build_fixture_setup_in(root, label, Some(&declaration_for(case)))?;
    if case.id == "DECL-MISSING" {
        // The shared setup writes the declaration; this case's state is an area
        // root with no declaration, so preparation removes exactly that file.
        let path = crate::area::declaration_path(&setup.state_paths, "oxide", "outputs")
            .map_err(|error| format!("the declaration path does not resolve: {error}"))?;
        std::fs::remove_file(&path)
            .map_err(|error| format!("the declaration cannot be removed: {error}"))?;
    }
    let layout = Layout::of(&setup);
    write_declared_state(&layout, case)?;
    let entries = observe_declared_state(&layout, case)?;
    let roots = declared_roots(&layout, case);
    let mut manifest = manifest_document(&setup, case.id);
    let expected: Vec<serde_json::Value> = case
        .expected_roots
        .iter()
        .map(|(slot, fate)| {
            let root = match *slot {
                "extra" => layout.extra_root.clone(),
                _ => layout.root.clone().unwrap_or_default(),
            };
            serde_json::json!({"root": root.display().to_string(), "fate": fate})
        })
        .collect();
    manifest["fixture"]["entries"] =
        serde_json::Value::Array(entries.iter().map(manifest_entry).collect());
    manifest["fixture"]["roots"] =
        serde_json::Value::Array(roots.iter().map(|root| serde_json::json!(root)).collect());
    let fixture_facts = observed_identity(&layout.dir)?;
    manifest["fixture"]["device"] = fixture_facts.0.into();
    manifest["fixture"]["inode"] = fixture_facts.1.into();
    manifest["fixture"]["mode"] = fixture_facts.2.into();
    manifest["expected"] =
        serde_json::json!({"classification": case.classification, "roots": expected});
    manifest["operator_list"] = if case
        .files
        .iter()
        .any(|file| file.slot == Slot::OperatorList && file.content == Content::OperatorList)
    {
        serde_json::json!(layout.resolve(Slot::OperatorList).display().to_string())
    } else {
        serde_json::Value::Null
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("the case manifest cannot encode: {error}"))?;
    crate::tape::validate_shared_manifest(&bytes)
        .map_err(|error| format!("the case manifest does not parse as /2: {error}"))?;
    let root = layout
        .root
        .clone()
        .unwrap_or_else(|| layout.dir.join("no-root"));
    Ok(PreparedCase {
        fixture: crate::tape_harness::EmbeddedFixture {
            dir: setup.dir,
            manifest: bytes,
            root,
        },
        spec: case,
        entries,
        roots,
    })
}

/// The declaration a case's fixture writes. `DECL-MISSING` writes one and then
/// removes it, so its area root still exists as the case's state.
fn declaration_for(case: &CaseSpec) -> crate::area::AreaDeclaration {
    let mut declaration = crate::tape_harness::base_declaration();
    if case.id == "DECL-POLICY-NONE" {
        declaration.prune = PrunePolicy::None;
    }
    declaration
}

/// Write only the case's declared paths. Nothing outside the fixture is touched.
fn write_declared_state(layout: &Layout, case: &CaseSpec) -> Result<(), String> {
    for file in case.files {
        let path = layout.resolve(file.slot);
        match file.content {
            Content::Absent => {
                // A declared absence is inspected, never created.
                if std::fs::symlink_metadata(&path).is_ok() {
                    return Err(format!(
                        "the fixture declares {} absent but it exists",
                        path.display()
                    ));
                }
                continue;
            }
            Content::Directory => {
                std::fs::create_dir_all(&path)
                    .map_err(|error| format!("{} cannot be created: {error}", path.display()))?;
                continue;
            }
            _ => {}
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{} cannot be created: {error}", parent.display()))?;
        }
        let bytes = document_bytes(layout, &file.content)?;
        std::fs::write(&path, bytes)
            .map_err(|error| format!("{} cannot be written: {error}", path.display()))?;
    }
    Ok(())
}

/// The bytes one document holds, derived only from the fixture's own paths and
/// observed identity.
fn document_bytes(layout: &Layout, content: &Content) -> Result<Vec<u8>, String> {
    let root = layout
        .root
        .as_deref()
        .ok_or_else(|| "the case's document needs an owned root".to_owned())?;
    let root_facts = observed_identity(root)?;
    match content {
        Content::Registry(shape) => registry_bytes(layout, root, root_facts, *shape),
        Content::Receipt(shape) => receipt_bytes(layout, root, *shape),
        Content::Marker(shape) => marker_bytes(layout, *shape),
        Content::Control(shape) => Ok(control_bytes(*shape)),
        Content::OperatorList => Ok(serde_json::to_vec(&serde_json::json!([root
            .display()
            .to_string()]))
        .map_err(|error| format!("the operator list cannot encode: {error}"))?),
        Content::BargeIntent => barge_intent_bytes(layout, root, root_facts),
        Content::Directory | Content::Absent => {
            Err("a directory or an absence carries no document".to_owned())
        }
    }
}

/// A path's own `(device, inode, mode, size)`.
fn observed_identity(path: &Path) -> Result<(u64, u64, u32, u64), String> {
    let facts = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{} cannot be observed: {error}", path.display()))?;
    Ok((facts.dev(), facts.ino(), facts.mode(), facts.len()))
}

fn registry_bytes(
    layout: &Layout,
    root: &Path,
    root_facts: (u64, u64, u32, u64),
    shape: RegistryShape,
) -> Result<Vec<u8>, String> {
    let checkout = layout.checkout.display().to_string();
    let receipt = layout.resolve(Slot::Receipt).display().to_string();
    let root_path = root.display().to_string();
    let row = |checkout_id: &str, checkout: &str, overrides: serde_json::Value| {
        let mut row = serde_json::json!({
            "checkout_id": checkout_id,
            "checkout": checkout,
            "receipt": receipt,
            "member": "target",
            "root": root_path,
            "root_kind": "external",
            "recipe": "target-debug-v1",
            "generation": 1,
            "nonce": NONCE,
            "selected": false,
            "pending": false,
        });
        if let (Some(target), Some(source)) = (row.as_object_mut(), overrides.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        row
    };
    let document = |rows: Vec<serde_json::Value>, complete: bool, incomplete: serde_json::Value| {
        let mut document = serde_json::json!({
            "version": OXIDE_REGISTRY_V2,
            "schema": 2,
            "revision": 1,
            "complete": complete,
            "created_by": "oxide",
            "rows": rows,
        });
        if !incomplete.is_null() {
            document["incomplete"] = incomplete;
        }
        document
    };
    let encoded = match shape {
        RegistryShape::Base | RegistryShape::StaleNonce => serde_json::to_vec_pretty(&document(
            vec![row(CHECKOUT_ID, &checkout, serde_json::json!({}))],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::Incomplete | RegistryShape::Overflow => {
            let tag = if shape == RegistryShape::Incomplete {
                "cancelled"
            } else {
                "cancelled-overflow"
            };
            serde_json::to_vec_pretty(&document(
                vec![row(CHECKOUT_ID, &checkout, serde_json::json!({}))],
                false,
                serde_json::json!({"reason": tag, "cancelled": tag}),
            ))
        }
        RegistryShape::Malformed => Ok(b"{\"version\":\"oxide.outputs.registry/2\",".to_vec()),
        RegistryShape::Oversize => {
            let mut bytes = b"{\"version\":\"oxide.outputs.registry/2\",\"rows\":[".to_vec();
            bytes.resize(REGISTRY_CAP_BYTES + 1, b' ');
            Ok(bytes)
        }
        RegistryShape::Contradictory => serde_json::to_vec_pretty(&document(
            vec![
                row(
                    CHECKOUT_ID,
                    &checkout,
                    serde_json::json!({"selected": true}),
                ),
                row(
                    OTHER_CHECKOUT_ID,
                    &layout.dir.join("other-checkout").display().to_string(),
                    serde_json::json!({"selected": true}),
                ),
            ],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::Pending => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"pending": true}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::Reservation => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"reservation": "tx-reservation"}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::Cancelling => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"cancelling": "tx-cancel"}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::Pruning => {
            let mut document = document(
                vec![row(
                    CHECKOUT_ID,
                    &checkout,
                    serde_json::json!({"pruning": "intent-1"}),
                )],
                true,
                serde_json::Value::Null,
            );
            document["intents"] = serde_json::json!([{
                "version": OXIDE_INTENT_V1,
                "id": "22222222222222222222222222222222",
                "identity": {
                    "checkout_id": CHECKOUT_ID,
                    "checkout": checkout,
                    "receipt": receipt,
                    "member": "target",
                    "root": root_path,
                    "root_kind": "external",
                    "generation": 1,
                    "dev": root_facts.0,
                    "ino": root_facts.1,
                    "marker_digest": RECORDED_MARKER_DIGEST.repeat(64),
                },
                "phase": "renamed",
                "quarantine": layout.pool.join("quarantine").display().to_string(),
                "preview_sha256": "0".repeat(64),
                "created_ms": 1,
                "updated_ms": 2,
            }]);
            serde_json::to_vec_pretty(&document)
        }
        RegistryShape::SelectedNotCurrent => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"selected": true}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::Empty => {
            serde_json::to_vec_pretty(&document(Vec::new(), true, serde_json::Value::Null))
        }
        RegistryShape::StaleGeneration => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"generation": 2}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::StaleInode => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"root_ino": root_facts.1 + 1}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::StaleDevice => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"root_dev": root_facts.0 + 1}),
            )],
            true,
            serde_json::Value::Null,
        )),
        RegistryShape::ReplacedMarker => serde_json::to_vec_pretty(&document(
            vec![row(
                CHECKOUT_ID,
                &checkout,
                serde_json::json!({"marker_digest": RECORDED_MARKER_DIGEST.repeat(64)}),
            )],
            true,
            serde_json::Value::Null,
        )),
    };
    encoded.map_err(|error| format!("the registry cannot encode: {error}"))
}

fn receipt_bytes(layout: &Layout, root: &Path, shape: ReceiptShape) -> Result<Vec<u8>, String> {
    let checkout = layout.checkout.display().to_string();
    let root_path = root.display().to_string();
    let encoded = match shape {
        ReceiptShape::Malformed => Ok(b"{\"version\":\"oxide.outputs.transition/2\",".to_vec()),
        ReceiptShape::V1 => serde_json::to_vec_pretty(&serde_json::json!({
            "version": OXIDE_RECEIPT_V1,
            "checkout": checkout,
            "checkout_id": CHECKOUT_ID,
            "generation": 1,
            "state": "committed",
            "selected": {"kind": "external", "root": root_path, "generation": 1},
            "roots": [],
            "updated_ms": 9,
        })),
        ReceiptShape::Base
        | ReceiptShape::NonCommitted
        | ReceiptShape::Contradictory
        | ReceiptShape::NonSelecting => {
            let state = if shape == ReceiptShape::NonCommitted {
                "prepared"
            } else {
                "committed"
            };
            let checkout_id = if shape == ReceiptShape::Contradictory {
                OTHER_CHECKOUT_ID
            } else {
                CHECKOUT_ID
            };
            let selection = if matches!(
                shape,
                ReceiptShape::NonCommitted | ReceiptShape::NonSelecting
            ) {
                serde_json::json!([{"member": "target"}])
            } else {
                serde_json::json!([{
                    "member": "target",
                    "selected": {"kind": "external", "root": root_path, "generation": 1},
                }])
            };
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": OXIDE_RECEIPT_V2,
                "checkout": checkout,
                "checkout_id": checkout_id,
                "generation": 1,
                "state": state,
                "links": selection,
                "roots": [],
                "updated_ms": 9,
            }))
        }
    };
    encoded.map_err(|error| format!("the receipt cannot encode: {error}"))
}

fn marker_bytes(layout: &Layout, shape: MarkerShape) -> Result<Vec<u8>, String> {
    let checkout = layout.checkout.display().to_string();
    let nonce = if shape == MarkerShape::OtherNonce {
        OTHER_NONCE
    } else {
        NONCE
    };
    let version = if shape == MarkerShape::V1 {
        OXIDE_MARKER_V1
    } else {
        OXIDE_MARKER_V2
    };
    match shape {
        MarkerShape::Malformed => Ok(b"{\"version\":\"oxide.outputs.root/2\",".to_vec()),
        MarkerShape::Base | MarkerShape::V1 | MarkerShape::OtherNonce | MarkerShape::Replaced => {
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": version,
                "checkout": checkout,
                "checkout_id": CHECKOUT_ID,
                "label": "target",
                "recipe": "target-debug-v1",
                "kind": "external",
                "nonce": nonce,
                "generation": 1,
                "created_ms": 1,
            }))
            .map_err(|error| format!("the marker cannot encode: {error}"))
        }
    }
}

fn control_bytes(shape: ControlShape) -> Vec<u8> {
    let owned_outputs = if shape == ControlShape::UnknownFence {
        "oxide.outputs/unknown"
    } else {
        "oxide.outputs/3"
    };
    format!(
        r#"{{"version":9,"next_id":9,"next_use":1,"tickets":[],"cleans":[],"records":[],"owned_outputs":"{owned_outputs}","recorded_accounting":"oxide.accounting/1","eviction_accounting":"oxide.eviction-accounting/1","slot_ceiling":"oxide.slots/256","holding":"oxide.holding/1"}}"#
    )
    .into_bytes()
}

/// One pending `barge.intent/2` record naming the owned root, with the facts this
/// preparation actually observed.
fn barge_intent_bytes(
    layout: &Layout,
    root: &Path,
    root_facts: (u64, u64, u32, u64),
) -> Result<Vec<u8>, String> {
    let root_id = root
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| "the owned root has no UTF-8 name".to_owned())?;
    let marker_file = root.join(crate::area::ROOT_MARKER_FILE);
    let marker = std::fs::read(&marker_file)
        .map_err(|error| format!("the root marker cannot be read: {error}"))?;
    let record = crate::state::IntentRecord {
        schema: crate::contract::INTENT_SCHEMA.to_owned(),
        intent_id: INTENT_ID.to_owned(),
        volume_uuid: layout.uuid.clone(),
        mount_hint: layout.mount.clone(),
        root_dir: String::new(),
        folder: "oxide-outputs".to_owned(),
        unit: crate::state::IntentUnit {
            parent: String::new(),
            name: root_id.to_owned(),
            kind: crate::unit::UnitKind::Directory,
            device: root_facts.0,
            inode: root_facts.1,
            marker: Some(crate::unit::UnitMarker::new(
                crate::unit::MarkerFamily::Barge,
                crate::digest::sha256_hex(&marker),
            )),
            // The unit's name is its root id here, so one lock covers both.
            root_id: None,
            capture_name: format!(".barge-prune-{INTENT_ID}"),
            grant: crate::unit::UnitGrant::native_root(root_id),
        },
        plan_digest: "0".repeat(64),
        phase: crate::state::IntentPhase::Intent,
        generation: 1,
        recorded_ms: 1_000,
        phase_ms: 1_000,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
        identity: None,
        absence: None,
    };
    record
        .validate()
        .map_err(|error| format!("the fixture intent does not validate: {error}"))?;
    let mut bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("the fixture intent cannot encode: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Observe every declared path, and only declared paths. The manifest's entries
/// are these observations, never a directory walk's extras.
fn observe_declared_state(layout: &Layout, case: &CaseSpec) -> Result<Vec<DeclaredEntry>, String> {
    let mut entries = Vec::new();
    for file in case.files {
        let path = layout.resolve(file.slot);
        let parent = path
            .parent()
            .ok_or_else(|| format!("{} has no parent", path.display()))?;
        let (parent_device, parent_inode, _, _) = observed_identity(parent).map_err(|error| {
            format!(
                "the declared parent of {} is not observable: {error}",
                path.display()
            )
        })?;
        let rel = path
            .strip_prefix(&layout.dir)
            .map_err(|_| format!("{} is outside the fixture", path.display()))?
            .display()
            .to_string();
        match std::fs::symlink_metadata(&path) {
            Ok(facts) if facts.is_dir() => entries.push(DeclaredEntry {
                shape: file.slot.shape(),
                rel,
                kind: "directory",
                device: Some(facts.dev()),
                inode: Some(facts.ino()),
                mode: Some(facts.mode()),
                size: Some(facts.len()),
                bytes_sha256: None,
                parent_device,
                parent_inode,
            }),
            Ok(facts) if facts.is_file() => {
                let bytes = std::fs::read(&path)
                    .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
                entries.push(DeclaredEntry {
                    shape: file.slot.shape(),
                    rel,
                    kind: "file",
                    device: Some(facts.dev()),
                    inode: Some(facts.ino()),
                    mode: Some(facts.mode()),
                    size: Some(facts.len()),
                    bytes_sha256: Some(crate::digest::sha256_hex(&bytes)),
                    parent_device,
                    parent_inode,
                });
            }
            Ok(_) => {
                return Err(format!(
                    "{} is neither a file nor a directory",
                    path.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if file.content != Content::Absent {
                    return Err(format!(
                        "the fixture declared {} present but it is absent",
                        path.display()
                    ));
                }
                entries.push(DeclaredEntry {
                    shape: file.slot.shape(),
                    rel,
                    kind: "absent",
                    device: None,
                    inode: None,
                    mode: None,
                    size: None,
                    bytes_sha256: None,
                    parent_device,
                    parent_inode,
                });
            }
            Err(error) => return Err(format!("{} cannot be observed: {error}", path.display())),
        }
    }
    Ok(entries)
}

/// The owned roots a case declares: the owned root, and the extra plan root.
fn declared_roots(layout: &Layout, case: &CaseSpec) -> Vec<String> {
    let mut roots = Vec::new();
    if case.expected_roots.iter().any(|(slot, _)| *slot == "extra") {
        roots.push(layout.extra_root.display().to_string());
    }
    if let Some(root) = &layout.root {
        roots.push(root.display().to_string());
    }
    roots
}

/// One manifest fixture entry, shaped by the /2 schema.
fn manifest_entry(entry: &DeclaredEntry) -> serde_json::Value {
    match entry.kind {
        "absent" => serde_json::json!({
            "rel": entry.rel,
            "parent_device": entry.parent_device,
            "parent_inode": entry.parent_inode,
            "kind": "absent",
        }),
        "directory" => serde_json::json!({
            "rel": entry.rel,
            "parent_device": entry.parent_device,
            "parent_inode": entry.parent_inode,
            "kind": "directory",
            "bytes_sha256": serde_json::Value::Null,
            "size": entry.size.unwrap_or(0),
            "device": entry.device.unwrap_or(0),
            "inode": entry.inode.unwrap_or(0),
            "mode": entry.mode.unwrap_or(0),
        }),
        _ => serde_json::json!({
            "rel": entry.rel,
            "parent_device": entry.parent_device,
            "parent_inode": entry.parent_inode,
            "kind": "file",
            "bytes_sha256": entry.bytes_sha256.clone().unwrap_or_default(),
            "size": entry.size.unwrap_or(0),
            "device": entry.device.unwrap_or(0),
            "inode": entry.inode.unwrap_or(0),
            "mode": entry.mode.unwrap_or(0),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The inventory is the D8/CF one: 50 cases, seven deferred, five
    /// unsupported, and no duplicate id.
    #[test]
    fn the_inventory_is_complete_and_unique() {
        assert_eq!(CASES.len(), 50);
        let mut seen = std::collections::BTreeSet::new();
        for case in CASES {
            assert!(seen.insert(case.id), "case {} appears twice", case.id);
        }
        let deferred = CASES
            .iter()
            .filter(|case| matches!(case.support, Support::Deferred(_)))
            .count();
        let unsupported = CASES
            .iter()
            .filter(|case| matches!(case.support, Support::Unsupported(_)))
            .count();
        assert_eq!(deferred, 7);
        assert_eq!(unsupported, 6);
        assert_eq!(CASES.len() - deferred - unsupported, 37);
        // The deferred set is exactly the writers/custody and admission families.
        for id in [
            "WRT-LIVE-THIS",
            "WRT-LIVE-OTHER",
            "WRT-UNKNOWN",
            "WRT-ACQUIRING",
            "WRT-RETIRED-CLEAN",
            "EFFECT-1",
            "EFFECT-2",
        ] {
            assert!(
                matches!(spec(id).unwrap().support, Support::Deferred(_)),
                "{id} is not deferred"
            );
        }
        // The unsupported set is exactly the cases whose fixture needs evidence
        // this preparation may not produce.
        for id in [
            "CAP-1",
            "ID-DEVICE-LIVE",
            "VOL-ABSENT",
            "VOL-REMOVED-DURING",
            "VOL-DUP-LIVE",
            "ABS-BOOT-DATA",
        ] {
            match spec(id).unwrap().support {
                Support::Unsupported(reason) => assert!(!reason.is_empty()),
                other => panic!("{id} is {other:?}, not unsupported"),
            }
        }
        assert!(matches!(
            spec("VOL-DUP-LIVE").unwrap().support,
            Support::Unsupported(_)
        ));
    }

    /// An unlisted case is refused, never served the BASE state.
    #[test]
    fn an_unlisted_case_is_refused() {
        assert!(spec("CTL-9").is_err());
        let error = prepare("tape-case-unlisted", "CTL-9").unwrap_err();
        assert!(error.contains("not a case"), "{error}");
    }

    /// Every supported case's specification differs from the BASE state, so no
    /// empty or BASE-shaped specification can masquerade as a requested non-BASE
    /// case.
    #[test]
    fn no_non_base_case_shares_the_base_state() {
        let base = spec("CTL-0").unwrap();
        for case in CASES {
            if case.id == "CTL-0" || !matches!(case.support, Support::Prepared) {
                continue;
            }
            assert_ne!(
                case.files, base.files,
                "case {} shares the BASE file list",
                case.id
            );
            assert!(
                !case.files.is_empty(),
                "case {} declares no state at all",
                case.id
            );
        }
    }

    /// Every prepared case declares at least one path, and every declared path
    /// resolves to a distinct location within the case.
    #[test]
    fn every_prepared_case_declares_distinct_paths() {
        for case in CASES {
            if !matches!(case.support, Support::Prepared) {
                continue;
            }
            assert!(!case.files.is_empty(), "{} declares no paths", case.id);
            let mut slots = std::collections::BTreeSet::new();
            for file in case.files {
                assert!(
                    slots.insert(format!("{:?}", file.slot)),
                    "{} declares {:?} twice",
                    case.id,
                    file.slot
                );
            }
        }
    }

    /// Every family names every CF with a reason, and every prepared case's
    /// obligation set is non-empty and names a slot kind.
    #[test]
    fn the_plan_obligations_and_cf_are_complete() {
        for case in CASES {
            assert_eq!(case.cf.len(), 7, "{} does not name all seven CFs", case.id);
            for note in case.cf {
                assert!(!note.reason.is_empty());
            }
            if matches!(case.support, Support::Prepared) {
                assert!(
                    !case.obligations.is_empty(),
                    "{} obliges no decision site",
                    case.id
                );
                for obligation in case.obligations {
                    assert_eq!(obligation.engine, "barge");
                    assert!(
                        obligation.site.contains('#'),
                        "{} is not a wire site",
                        obligation.site
                    );
                }
            }
        }
    }

    /// The plan is effect-free: it prints every case, its status, its prepared
    /// relative paths (never an absolute path) and its obligations, and it
    /// counts exactly.
    #[test]
    fn the_plan_prints_every_spec_and_obligation() {
        let document = plan_document();
        let text = serde_json::to_string(&document).unwrap();
        assert!(!text.contains("/Volumes"));
        assert!(!text.contains("/Users"));
        assert_eq!(document["document"], PLAN_DOCUMENT);
        assert_eq!(document["runnable"], 0);
        assert_eq!(document["counts"]["total"], 50);
        assert_eq!(document["counts"]["prepared"], 37);
        assert_eq!(document["counts"]["unsupported"], 6);
        assert_eq!(document["counts"]["deferred"], 7);
        let rows = document["cases"].as_array().unwrap();
        assert_eq!(rows.len(), 50);
        let ctl = rows.iter().find(|row| row["id"] == "CTL-0").unwrap();
        assert_eq!(ctl["status"], "insufficient");
        assert_eq!(ctl["fixture"], "prepared");
        assert!(ctl["obligations"].as_array().unwrap().len() >= 3);
        let effect = rows.iter().find(|row| row["id"] == "EFFECT-1").unwrap();
        assert_eq!(effect["status"], "deferred");
        assert_eq!(effect["fixture"], "not prepared");
        let cap = rows.iter().find(|row| row["id"] == "CAP-1").unwrap();
        assert!(cap["fixture"].as_str().unwrap().contains("shared caps"));
        // Printing twice is the same document: the plan holds no state.
        assert_eq!(document, plan_document());
        eprintln!("{}", serde_json::to_string_pretty(&document).unwrap());
    }

    /// The setup control for the BASE-compatible case: the same registry bytes the
    /// existing `build_fixture` writes, and a manifest that parses as /2.
    #[test]
    fn the_base_case_prepares_the_base_state() {
        let prepared = prepare("tape-case-ctl0", "CTL-0").unwrap();
        crate::tape::validate_shared_manifest(&prepared.fixture.manifest).unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&prepared.fixture.manifest).unwrap();
        assert_eq!(document["case"], "CTL-0");
        assert_eq!(document["expected"]["classification"], "MATCH");
        assert_eq!(document["expected"]["roots"][0]["fate"], "release");
        let registry = prepared
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Registry.shape())
            .unwrap();
        let path = prepared.fixture.dir.path().join(&registry.rel);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            registry.bytes_sha256.as_deref(),
            Some(crate::digest::sha256_hex(&bytes).as_str())
        );
        assert!(String::from_utf8(bytes)
            .unwrap()
            .contains("oxide.outputs.registry/2"));
        // Every entry's digest, size and identity match the file on disk, and its
        // parent identity is the parent's own.
        for entry in &prepared.entries {
            let path = prepared.fixture.dir.path().join(&entry.rel);
            match entry.kind {
                "absent" => assert!(!path.exists()),
                "directory" => assert!(path.is_dir()),
                _ => {
                    let bytes = std::fs::read(&path).unwrap();
                    assert_eq!(
                        entry.bytes_sha256.as_deref(),
                        Some(crate::digest::sha256_hex(&bytes).as_str())
                    );
                    assert_eq!(entry.size, Some(bytes.len() as u64));
                }
            }
            let parent = path.parent().unwrap();
            let facts = std::fs::symlink_metadata(parent).unwrap();
            assert_eq!(entry.parent_device, facts.dev());
            assert_eq!(entry.parent_inode, facts.ino());
        }
        assert!(prepared
            .roots
            .iter()
            .any(|root| root == &prepared.fixture.root.display().to_string()));
    }

    /// Every prepared case materializes, its manifest parses as strict `/2`, and
    /// each declared entry's observed facts are the fixture's own. This is a
    /// per-case setup control over all 37 prepared specifications.
    #[test]
    fn every_prepared_case_materializes_and_parses() {
        for case in CASES {
            if !matches!(case.support, Support::Prepared) {
                continue;
            }
            let prepared = prepare(
                &format!("tape-case-all-{}", case.id.to_ascii_lowercase()),
                case.id,
            )
            .unwrap_or_else(|error| panic!("{} cannot be prepared: {error}", case.id));
            crate::tape::validate_shared_manifest(&prepared.fixture.manifest)
                .unwrap_or_else(|error| panic!("{} does not parse as /2: {error}", case.id));
            let document: serde_json::Value =
                serde_json::from_slice(&prepared.fixture.manifest).unwrap();
            assert_eq!(document["case"], case.id);
            assert_eq!(document["expected"]["classification"], case.classification);
            assert_eq!(
                document["expected"]["roots"].as_array().unwrap().len(),
                case.expected_roots.len(),
                "{}",
                case.id
            );
            // No prepared case claims an invented obligation identity, a writer or
            // an effect.
            assert!(document["mandatory_observations"]
                .as_array()
                .unwrap()
                .is_empty());
            assert!(document["dynamic"]["writers"]
                .as_array()
                .unwrap()
                .is_empty());
            assert!(document["expected_effects"].as_array().unwrap().is_empty());
            assert_eq!(
                document["fixture"]["entries"].as_array().unwrap().len(),
                case.files.len(),
                "{}",
                case.id
            );
            for (entry, file) in prepared.entries.iter().zip(case.files) {
                assert_eq!(entry.shape, file.slot.shape(), "{}", case.id);
                let path = prepared.fixture.dir.path().join(&entry.rel);
                match file.content {
                    Content::Absent => assert_eq!(entry.kind, "absent", "{}", case.id),
                    Content::Directory => assert!(path.is_dir(), "{}", case.id),
                    _ => {
                        let bytes = std::fs::read(&path).unwrap();
                        assert_eq!(
                            entry.bytes_sha256.as_deref(),
                            Some(crate::digest::sha256_hex(&bytes).as_str()),
                            "{}",
                            case.id
                        );
                    }
                }
                let facts = std::fs::symlink_metadata(path.parent().unwrap()).unwrap();
                assert_eq!(entry.parent_device, facts.dev(), "{}", case.id);
                assert_eq!(entry.parent_inode, facts.ino(), "{}", case.id);
            }
        }
    }

    /// The registry family's distinct states are genuinely different: missing,
    /// malformed, oversized, contradictory and sticky-incomplete.
    #[test]
    fn the_registry_cases_prepare_distinct_state() {
        let missing = prepare("tape-case-reg-missing", "REG-MISSING").unwrap();
        let missing_registry = missing
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Registry.shape());
        assert!(
            missing_registry.is_none(),
            "REG-MISSING must not declare a registry"
        );
        let outputs = missing
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::OutputsDir.shape())
            .unwrap();
        assert_eq!(outputs.kind, "absent");
        assert!(!missing.fixture.dir.path().join(&outputs.rel).exists());

        let malformed = prepare("tape-case-reg-malformed", "REG-MALFORMED").unwrap();
        let bytes =
            std::fs::read(malformed.fixture.dir.path().join(&malformed.entries[2].rel)).unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_err());

        let oversize = prepare("tape-case-reg-oversize", "REG-OVERSIZE").unwrap();
        let entry = oversize
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Registry.shape())
            .unwrap();
        assert!(entry.size.unwrap() > REGISTRY_CAP_BYTES as u64);

        let incomplete = prepare("tape-case-reg-incomplete", "CTL-1").unwrap();
        let bytes = std::fs::read(
            incomplete
                .fixture
                .dir
                .path()
                .join(&incomplete.entries[2].rel),
        )
        .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(document["complete"], false);
        assert!(document["incomplete"].is_object());

        // The BASE and the incomplete case are not the same bytes, so an empty or
        // BASE-shaped state cannot stand in for CTL-1.
        let base = prepare("tape-case-ctl0-compare", "CTL-0").unwrap();
        let base_bytes = std::fs::read(base.fixture.dir.path().join(&base.entries[2].rel)).unwrap();
        assert_ne!(base_bytes, bytes);
    }

    /// The references family writes a real pending Barge intent that Barge's own
    /// reader reads as pending, and the registry shapes carry their flags.
    #[test]
    fn the_reference_cases_prepare_their_rows_and_intents() {
        let prepared = prepare("tape-case-ref-pruning", "REF-PRUNING").unwrap();
        let intent = prepared
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Intent.shape())
            .unwrap();
        assert_eq!(intent.kind, "file");
        let state = crate::state::StatePaths::resolve(
            &crate::testkit::FakeAccount {
                home: prepared.fixture.dir.path().to_path_buf(),
            },
            Some(&prepared.fixture.dir.path().join("state")),
        )
        .unwrap();
        let intents = crate::state::read_intents(&state, crate::contract::INTENT_ROWS).unwrap();
        assert_eq!(intents.records.len(), 1);
        assert!(intents.records[0].is_pending());
        // The registry row is pruning, with its own unfinished journal entry.
        let registry =
            std::fs::read(prepared.fixture.dir.path().join(&prepared.entries[2].rel)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&registry).unwrap();
        assert_eq!(document["rows"][0]["pruning"], "intent-1");
        assert_eq!(document["intents"][0]["phase"], "renamed");

        for (id, flag) in [
            ("REF-PENDING", "pending"),
            ("REF-RESERVATION", "reservation"),
            ("REF-CANCELLING", "cancelling"),
            ("REF-SELECTED-NOT-CURRENT", "selected"),
        ] {
            let prepared = prepare(&format!("tape-case-{}", id.to_lowercase()), id).unwrap();
            let registry =
                std::fs::read(prepared.fixture.dir.path().join(&prepared.entries[2].rel)).unwrap();
            let document: serde_json::Value = serde_json::from_slice(&registry).unwrap();
            assert!(
                !document["rows"][0][flag].is_null() && document["rows"][0][flag] != false,
                "{id} does not set {flag}"
            );
        }
    }

    /// The receipts and markers family writes the distinct documents it names,
    /// and the absent ones are observed absent.
    #[test]
    fn the_receipt_and_marker_cases_prepare_distinct_documents() {
        let missing = prepare("tape-case-rcp-missing", "RCP-MISSING").unwrap();
        let receipt = missing
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Receipt.shape())
            .unwrap();
        assert_eq!(receipt.kind, "absent");
        let marker = missing
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Marker.shape())
            .unwrap();
        assert_eq!(marker.kind, "file");

        let v1 = prepare("tape-case-rcp-v1", "RCP-V1").unwrap();
        let receipt = std::fs::read(v1.fixture.dir.path().join(&v1.entries[3].rel)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&receipt).unwrap();
        assert_eq!(document["version"], OXIDE_RECEIPT_V1);
        let marker = std::fs::read(v1.fixture.dir.path().join(&v1.entries[4].rel)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&marker).unwrap();
        assert_eq!(document["version"], OXIDE_MARKER_V1);

        let replaced = prepare("tape-case-mrk-replaced", "MRK-REPLACED").unwrap();
        let registry =
            std::fs::read(replaced.fixture.dir.path().join(&replaced.entries[2].rel)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&registry).unwrap();
        assert_eq!(
            document["rows"][0]["marker_digest"],
            RECORDED_MARKER_DIGEST.repeat(64)
        );
        // The marker's real digest is not the recorded one.
        let marker = replaced
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Marker.shape())
            .unwrap();
        assert_ne!(
            marker.bytes_sha256.as_deref(),
            Some(RECORDED_MARKER_DIGEST.repeat(64).as_str())
        );

        let missing_marker = prepare("tape-case-mrk-missing", "MRK-MISSING").unwrap();
        let marker = missing_marker
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::Marker.shape())
            .unwrap();
        assert_eq!(marker.kind, "absent");
    }

    /// The identity family records the mismatch on one side only, and the
    /// observed root identity is the root's own.
    #[test]
    fn the_identity_cases_record_a_stale_side() {
        let inode = prepare("tape-case-id-inode", "ID-INODE").unwrap();
        let root = &inode.fixture.root;
        let facts = std::fs::symlink_metadata(root).unwrap();
        let registry = std::fs::read(inode.fixture.dir.path().join(&inode.entries[2].rel)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&registry).unwrap();
        assert_eq!(
            document["rows"][0]["root_ino"].as_u64().unwrap(),
            facts.ino() + 1
        );
        let device = prepare("tape-case-id-device", "ID-DEVICE-STALE").unwrap();
        let device_facts = std::fs::symlink_metadata(&device.fixture.root).unwrap();
        let registry =
            std::fs::read(device.fixture.dir.path().join(&device.entries[2].rel)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&registry).unwrap();
        assert_eq!(
            document["rows"][0]["root_dev"].as_u64().unwrap(),
            device_facts.dev() + 1
        );
    }

    /// The fence and declaration cases write the control state they name and the
    /// declaration policy they declare.
    #[test]
    fn the_fence_and_declaration_cases_prepare_their_state() {
        let missing = prepare("tape-case-fence-missing", "FENCE-MISSING").unwrap();
        let state = missing
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::ControlState.shape())
            .unwrap();
        assert_eq!(state.kind, "absent");

        let unknown = prepare("tape-case-fence-unknown", "FENCE-UNKNOWN").unwrap();
        let bytes =
            std::fs::read(unknown.fixture.dir.path().join(&unknown.entries[3].rel)).unwrap();
        assert!(String::from_utf8(bytes)
            .unwrap()
            .contains("oxide.outputs/unknown"));

        let policy = prepare("tape-case-decl-policy", "DECL-POLICY-NONE").unwrap();
        let policy_state = crate::state::StatePaths::resolve(
            &crate::testkit::FakeAccount {
                home: policy.fixture.dir.path().to_path_buf(),
            },
            Some(&policy.fixture.dir.path().join("state")),
        )
        .unwrap();
        let document = crate::area::show(&policy_state, "oxide", "outputs")
            .unwrap()
            .expect("the declaration is present");
        assert!(matches!(document.prune, PrunePolicy::None));

        let undeclared = prepare("tape-case-decl-missing", "DECL-MISSING").unwrap();
        let state = crate::state::StatePaths::resolve(
            &crate::testkit::FakeAccount {
                home: undeclared.fixture.dir.path().to_path_buf(),
            },
            Some(&undeclared.fixture.dir.path().join("state")),
        )
        .unwrap();
        assert!(crate::area::show(&state, "oxide", "outputs")
            .unwrap()
            .is_none());
    }

    /// The legacy family writes the operator list where it is listed and leaves
    /// it absent where it is not.
    #[test]
    fn the_legacy_cases_prepare_their_operator_list() {
        let listed = prepare("tape-case-leg-listed", "LEG-LISTED").unwrap();
        let entry = listed
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::OperatorList.shape())
            .unwrap();
        assert_eq!(entry.kind, "file");
        let bytes = std::fs::read(listed.fixture.dir.path().join(&entry.rel)).unwrap();
        let roots: Vec<String> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(roots, vec![listed.fixture.root.display().to_string()]);
        let document: serde_json::Value = serde_json::from_slice(&listed.fixture.manifest).unwrap();
        assert!(document["operator_list"].is_string());

        let unlisted = prepare("tape-case-leg-unlisted", "LEG-UNLISTED").unwrap();
        let entry = unlisted
            .entries
            .iter()
            .find(|entry| entry.shape == Slot::OperatorList.shape())
            .unwrap();
        assert_eq!(entry.kind, "absent");
        let document: serde_json::Value =
            serde_json::from_slice(&unlisted.fixture.manifest).unwrap();
        assert!(document["operator_list"].is_null());
    }

    /// Unsupported and deferred cases refuse, name their reason and leave nothing
    /// behind: the fixture directory is removed on the refused path.
    #[test]
    fn refused_cases_name_their_reason_and_leave_no_fixture() {
        let parent = crate::testkit::TempDir::new("tape-case-refusal-root");
        let run_root = crate::testkit::RunRoot {
            path: parent.path().to_path_buf(),
            owned: false,
        };
        for (id, expected) in [
            ("ID-DEVICE-LIVE", "second mount epoch"),
            ("VOL-DUP-LIVE", "duplicate-UUID"),
            ("ABS-BOOT-DATA", "boot-Data"),
            ("CAP-1", "shared caps"),
            ("WRT-LIVE-THIS", "deliberately held"),
            ("EFFECT-2", "deliberately held"),
        ] {
            let error = prepare_in(&run_root, "tape-case-refused", id).unwrap_err();
            assert!(
                error.contains(expected),
                "{id} refused without its reason: {error}"
            );
            assert_eq!(
                std::fs::read_dir(&run_root.path).unwrap().count(),
                0,
                "{id} left a fixture behind"
            );
        }
    }

    /// Preparation opens no window and runs no engine: the module names neither
    /// the harness runner nor the planner.
    #[test]
    fn preparation_opens_no_window() {
        // The guard covers the preparation itself, not this control's own text.
        let source = include_str!("tape_cases.rs")
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or_default();
        for forbidden in [
            "release::plan(",
            "begin_window(",
            "run_bound(",
            "tape_harness::run(",
            "release::execute(",
        ] {
            assert!(
                !source.contains(forbidden),
                "the fixture preparation names `{forbidden}`"
            );
        }
    }
}
