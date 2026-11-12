//! Oxide's pool-ledger writer custody, read in place (stage 4 `R4`).
//!
//! A release may delete a root only when every writer that could still write
//! into that root's checkout is positively closed. Oxide's rule is "every writer
//! positively closed across both pools" (`workflow.md`, pruning owned output
//! roots). Oxide keeps that fact in two documents per pool, as `local-lease.md`
//! describes them:
//!
//! - `<pool>/oxide-managed/state.json`: the front-door records, one per slot,
//!   each with the checkout it serves, its phase, and any lost slot's unclosed
//!   writers. Oxide's own custody check (`checkout_custody`) reads this file.
//! - `<pool>/ledger.json`: the library ledger, one row per slot. A held row is
//!   bound to its front-door record through its affinity directory.
//!
//! This reader never writes, takes no lock (not even Oxide's `lock`), creates no
//! file, follows no link and repairs nothing. Each pool is read once per plan and
//! every checkout is answered from that one reading.
//!
//! Each checkout gets one of three answers:
//!
//! - [`Custody::Closed`]: the reading positively shows no writer for it;
//! - [`Custody::Held`]: a writer, an acquiring lease or a running clean names it;
//! - [`Custody::Unknown`]: a lost, unreadable, malformed, oversized, unknown-version
//!   or mid-read-changed fact, or a record that cannot be read as closed; a
//!   coordinated `/5` handoff that is one-sided or unfinished is one of them.
//!
//! The covered owned-outputs fences are `oxide.outputs/3`–`/5`. `/5` is the
//! coordinated single-authority handoff: both controls carry it with one
//! completed `ready` transaction, and the `/4` registry image records that
//! transaction as its `created_by` (see [`CoordinatedAdoption`]).
//!
//! Unknown keeps. A fact that cannot be tied to one checkout (a held row with no
//! record, an active or unclosed record that names no checkout, an unreadable
//! pool) is pool-wide unknown, and the caller keeps every root that the reading
//! covers.
//!
//! Where this reader is stricter than Oxide's own check, the difference keeps
//! more, never less:
//!
//! - a retired record without a `lost` fact is unknown (Oxide ignores it);
//! - an unclosed lost slot is unknown whatever its holder's boot session (Oxide
//!   may close it by restart; this reader does not read the boot session);
//! - an acquiring record and a running clean are held (Oxide counts only
//!   spawning and running records).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::de::IgnoredAny;
use serde::Deserialize;

use crate::state::{self, Bounds, Deadline, ReadOutcome};

/// The source family a control-state reading names.
pub const CONTROL_SOURCE: &str = "oxide.pool.control";
/// The source family a pool-ledger reading names.
pub const LEDGER_SOURCE: &str = "oxide.pool.ledger";
/// Oxide's control directory inside a pool directory.
pub const CONTROL_DIR: &str = "oxide-managed";
/// The control state document inside [`CONTROL_DIR`].
pub const STATE_FILE: &str = "state.json";
/// The library ledger inside a pool directory.
pub const LEDGER_FILE: &str = "ledger.json";
/// The removable pool's directory, inside the internal pool directory.
pub const REMOVABLE_POOL_DIR: &str = "removable";
/// The affinity directory under [`CONTROL_DIR`] that binds a slot to its record.
const AFFINITY_DIR: &str = "affinity";

/// Oxide's control-state byte cap (`MAX_STATE_BYTES`).
pub const STATE_CAP_BYTES: usize = 32 * 1024 * 1024;
/// Oxide's ledger byte cap (`MAX_LEDGER_BYTES`).
pub const LEDGER_CAP_BYTES: usize = 8 * 1024 * 1024;
/// Oxide's record ceiling (`MAX_RECORDS`).
pub const MAX_RECORDS: usize = 512;
/// Oxide's slot ceiling (`MAX_SLOTS`).
pub const MAX_SLOTS: usize = 256;
/// Oxide's ticket ceiling (`MAX_TICKETS`).
pub const MAX_TICKETS: usize = 128;
/// Oxide's clean-mark ceiling (`MAX_CLEANS`).
pub const MAX_CLEANS: usize = 128;
/// Oxide's recovery ceiling (`MAX_RECOVERIES`).
pub const MAX_RECOVERIES: usize = 16;
/// Oxide's flush ceiling (`MAX_FLUSHES`).
pub const MAX_FLUSHES: usize = 16;

/// The control-state versions this reader accepts; Oxide's own range.
const STATE_VERSIONS: std::ops::RangeInclusive<u32> = 1..=9;
/// The library ledger version Oxide writes (`LEASE_VERSION`).
const LEDGER_VERSION: u32 = 1;
/// The capacity-policy fences Oxide's ledger writes (`1..=4`).
const CAPACITY_POLICIES: std::ops::RangeInclusive<u32> = 1..=4;

const OWNED_OUTPUTS_FENCES: [&str; 5] = [
    "oxide.outputs/1",
    "oxide.outputs/2",
    "oxide.outputs/3",
    "oxide.outputs/4",
    "oxide.outputs/5",
];

/// The owned-outputs fence the covered delegation class requires.
///
/// Oxide's successor fence `oxide.outputs/3` is the one a ready registry is
/// written behind, and the only one that advances a pool's numeric state
/// version past its predecessors; the published `/2` and released `/1` fences
/// are legacy and never covered.
pub const COVERED_OWNED_OUTPUTS_FENCE: &str = "oxide.outputs/3";

/// The explicit deletion-route fence; it does not raise the numeric state version.
pub const ROUTED_OWNED_OUTPUTS_FENCE: &str = "oxide.outputs/4";

/// Oxide's coordinated single-authority fence (its own `COORDINATED_OUTPUTS_FENCE`).
///
/// It is written by the explicit forward adoption only, keeps the numeric state
/// floor of its `/4` predecessor, and always travels with one completed paired
/// initialization whose transaction also created the `/4` registry image. A
/// `/6` or any other value is not this reader's document and stays unknown.
pub const COORDINATED_OWNED_OUTPUTS_FENCE: &str = "oxide.outputs/5";

/// The numeric state floor every covered fence implies.
const COVERED_STATE_FLOOR: u32 = 4;

/// Oxide's byte cap for the outputs-initialization transaction
/// (`MAX_INIT_TRANSACTION_BYTES`).
const INIT_TRANSACTION_CAP_BYTES: usize = 64;

/// Whether the pools' control states carry the covered class's fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputsFence {
    /// Every pool that was read parses and carries a covered fence. A
    /// coordinated `/5` handoff also reports the completed transaction its
    /// registry image must have been created by.
    Covered {
        /// The paired coordinated adoption, when the controls carry `/5`;
        /// `None` for the historical `/3` and `/4` classes.
        coordinated: Option<CoordinatedAdoption>,
    },
    /// The pools were read completely and at least one is not on the covered
    /// class: a legacy fence, or no fence at all. A known not-covered is a
    /// refusal, never an unknown.
    NotCovered {
        /// Which pool, and what it carries.
        reason: String,
    },
    /// A pool's state could not be read as a document this build understands,
    /// or the read did not finish: an unknown fence is unknown, never absent.
    Unknown {
        /// Why, for a human.
        reason: String,
    },
}

/// Oxide's completed coordinated outputs adoption, as the pool controls record it.
///
/// The producer writes both controls' `/5` fence and `initializing` record under
/// one transaction, publishes the atomic `/4` registry image, and only then
/// records `ready` on each control. A `/5` control without that completed
/// record, or a pair whose transactions differ, is an incomplete observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatedAdoption {
    transaction: String,
}

impl CoordinatedAdoption {
    /// The transaction both controls record `ready` with, and that the
    /// coordinated registry image records as its `created_by`.
    pub fn transaction(&self) -> &str {
        &self.transaction
    }
}

/// One control document's owned-outputs facts, as the pairing rules read them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PoolFence {
    pool: PathBuf,
    version: u32,
    fence: Option<String>,
    /// The completed transaction when the control records `ready`.
    ready: Option<String>,
}

impl PoolFence {
    fn of(pool: &Path, state: &RawState) -> Self {
        Self {
            pool: pool.to_path_buf(),
            version: state.version,
            fence: state.owned_outputs.clone(),
            ready: state
                .outputs_init
                .as_ref()
                .and_then(RawOutputsInit::ready_transaction)
                .map(str::to_owned),
        }
    }
}

/// What the controls that were read say about Oxide's coordinated adoption.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Coordination {
    /// No control carries the coordinated fence.
    Absent,
    /// Every control carries it with one completed ready transaction.
    Ready(CoordinatedAdoption),
    /// The observation is one-sided, mid-adoption or contradictory.
    Unknown(String),
}

/// Bind the pools' own control documents into one adoption decision.
///
/// `/5` is a paired handoff: every control that was read must carry it, at the
/// covered numeric floor, with one completed `ready` transaction. A control
/// that still says `/4`, a pool with no control document, or two different
/// transactions is a one-sided or mid-adoption observation and never a
/// covered pair; unknown keeps every root the reading covers.
fn coordinate(observed: &[PoolFence], pools: usize) -> Coordination {
    let coordinated: Vec<&PoolFence> = observed
        .iter()
        .filter(|fact| fact.fence.as_deref() == Some(COORDINATED_OWNED_OUTPUTS_FENCE))
        .collect();
    if coordinated.is_empty() {
        return Coordination::Absent;
    }
    if coordinated.len() != pools {
        return Coordination::Unknown(format!(
            "only {} of {pools} pool controls record the coordinated fence {COORDINATED_OWNED_OUTPUTS_FENCE}, so the paired adoption is incomplete",
            coordinated.len()
        ));
    }
    if coordinated
        .iter()
        .any(|fact| fact.version < COVERED_STATE_FLOOR)
    {
        return Coordination::Unknown(format!(
            "a pool control carries {COORDINATED_OWNED_OUTPUTS_FENCE} below version {COVERED_STATE_FLOOR}, which no state Oxide writes does"
        ));
    }
    let Some(first) = coordinated.first() else {
        return Coordination::Absent;
    };
    let Some(transaction) = first.ready.as_deref() else {
        return Coordination::Unknown(format!(
            "{} carries the coordinated fence without a completed ready initialization",
            first.pool.display()
        ));
    };
    if let Some(other) = coordinated
        .iter()
        .find(|fact| fact.ready.as_deref() != Some(transaction))
    {
        return Coordination::Unknown(format!(
            "{} and {} record different coordinated transactions, so no one adoption is established",
            first.pool.display(),
            other.pool.display()
        ));
    }
    Coordination::Ready(CoordinatedAdoption {
        transaction: transaction.to_owned(),
    })
}

/// Read every pool's control state and decide the owned-outputs fence.
///
/// Nothing is created, locked or repaired. The decision is conjunctive: one
/// unreadable pool is unknown for the whole set, and one legacy or fenceless
/// pool keeps the whole set out of the covered class. The coordinated `/5`
/// fence is decided as the pair it is: every pool control must carry it with
/// one completed `ready` transaction, and a mixed, unequal or one-sided pair
/// is unknown rather than covered.
pub fn owned_outputs_fence(pools: &[PathBuf], deadline: Deadline) -> OutputsFence {
    let mut observed: Vec<PoolFence> = Vec::with_capacity(pools.len());
    for pool in pools {
        let control = pool.join(CONTROL_DIR);
        let path = control.join(STATE_FILE);
        let bytes = match read_document(&path, STATE_CAP_BYTES, deadline) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return OutputsFence::Unknown {
                    reason: format!(
                        "{} is not there, so the pool's owned-outputs fence is unknown",
                        path.display()
                    ),
                }
            }
            Err(error) => {
                return OutputsFence::Unknown {
                    reason: format!("{} cannot be read: {error}", path.display()),
                }
            }
        };
        let state = match parse_state(&bytes) {
            Ok(state) => state,
            Err(reason) => {
                return OutputsFence::Unknown {
                    reason: format!(
                        "{} is not a control state this build reads: {reason}",
                        path.display()
                    ),
                }
            }
        };
        match state.owned_outputs.as_deref() {
            Some(fence)
                if [
                    COVERED_OWNED_OUTPUTS_FENCE,
                    ROUTED_OWNED_OUTPUTS_FENCE,
                    COORDINATED_OWNED_OUTPUTS_FENCE,
                ]
                .contains(&fence) =>
            {
                // The successor fence is the one that advances the numeric
                // state version past its predecessors, so a covered pool is
                // always at version 4 or later; the other fences' own
                // version pairing was checked by the parse.
                if state.version < COVERED_STATE_FLOOR {
                    return OutputsFence::Unknown {
                        reason: format!(
                            "{} carries the covered fence {fence} at version {}, which no state Oxide writes does",
                            path.display(),
                            state.version
                        ),
                    };
                }
                observed.push(PoolFence::of(pool, &state));
            }
            Some(fence) => {
                return OutputsFence::NotCovered {
                    reason: format!(
                        "{} carries the legacy fence {fence}, which is not the covered class",
                        path.display()
                    ),
                }
            }
            None => {
                return OutputsFence::NotCovered {
                    reason: format!(
                        "{} records no owned-outputs fence, so its pool never opted in and is not the covered class",
                        path.display()
                    ),
                }
            }
        }
    }
    match coordinate(&observed, pools.len()) {
        Coordination::Absent => OutputsFence::Covered { coordinated: None },
        Coordination::Ready(adoption) => OutputsFence::Covered {
            coordinated: Some(adoption),
        },
        Coordination::Unknown(reason) => OutputsFence::Unknown { reason },
    }
}
const RECORDED_ACCOUNTING_FENCE: &str = "oxide.accounting/1";
const ONE_USE_FENCE: &str = "oxide.one-use/1";
const EVICTION_ACCOUNTING_FENCE: &str = "oxide.eviction-accounting/1";
const SLOT_CEILING_FENCE: &str = "oxide.slots/256";
const HOLDING_FENCE: &str = "oxide.holding/1";

/// One checkout's custody, as this reader answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Custody {
    /// The reading positively shows no writer for this checkout.
    Closed,
    /// A writer, an acquiring lease or a running clean names this checkout.
    Held(String),
    /// Some fact about this checkout could not be read as closed.
    Unknown(String),
}

/// The custody reading of one or more Oxide pools, read once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolCustody {
    /// The pool directories that were read, in the order given.
    pools: Vec<PathBuf>,
    /// Facts each checkout's verdict is built from.
    per_checkout: BTreeMap<PathBuf, Entries>,
    /// Facts no single checkout owns: an unreadable pool, a held row with no
    /// record. Any of them keeps every root the reading covers.
    unattributed: Vec<String>,
    /// The coordinated outputs adoption every pool control records ready, when
    /// they do. `None` for the historical `/1`–`/4` controls, whose own
    /// registry binding is not part of this reading.
    coordinated: Option<CoordinatedAdoption>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Entries {
    held: Vec<String>,
    unknown: Vec<String>,
}

impl PoolCustody {
    /// Read the custody of each pool directory, bounded by `deadline`.
    ///
    /// A pool directory is the one that holds `ledger.json` and `oxide-managed/`.
    /// Nothing is created, locked or repaired.
    pub fn read(pools: &[PathBuf], deadline: Deadline) -> Self {
        Self::read_with_caps(pools, deadline, STATE_CAP_BYTES, LEDGER_CAP_BYTES)
    }

    fn read_with_caps(
        pools: &[PathBuf],
        deadline: Deadline,
        state_cap: usize,
        ledger_cap: usize,
    ) -> Self {
        if pools.is_empty() {
            return Self::unknown("no pool was given, so no writer can be read as closed");
        }
        let mut reading = Self {
            pools: pools.to_vec(),
            ..Self::default()
        };
        let observed: Vec<PoolFence> = pools
            .iter()
            .filter_map(|pool| reading.read_pool(pool, deadline, state_cap, ledger_cap))
            .collect();
        // The paired `/5` handoff is decided over every pool that was given,
        // so a one-sided adoption is a pool-wide unknown and keeps the batch.
        match coordinate(&observed, pools.len()) {
            Coordination::Absent => {}
            Coordination::Ready(adoption) => reading.coordinated = Some(adoption),
            Coordination::Unknown(reason) => reading.unattributed.push(reason),
        }
        reading
    }

    /// A reading that could not be taken at all: every checkout is unknown
    /// through the pool-wide fact, so the caller keeps everything it covers.
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            unattributed: vec![reason.into()],
            ..Self::default()
        }
    }

    /// The pool directories this reading covers.
    pub fn pools(&self) -> &[PathBuf] {
        &self.pools
    }

    /// The coordinated outputs adoption both pool controls record ready, when
    /// they do. A `/5` reading never establishes one unless every control
    /// agrees, and such a control pair's registry image must be created by
    /// this transaction.
    pub fn coordinated(&self) -> Option<&CoordinatedAdoption> {
        self.coordinated.as_ref()
    }

    /// Why the reading as a whole is unknown, when a fact belongs to no single
    /// checkout. `None` when every unknown fact is attributable.
    pub fn unattributed(&self) -> Option<String> {
        (!self.unattributed.is_empty()).then(|| self.unattributed.join("; "))
    }

    /// The custody of one checkout. A pool-wide unknown makes every checkout
    /// unknown, so this answer alone is never closed while any fact is unknown.
    /// Unknown beats held, and held beats closed.
    pub fn for_checkout(&self, checkout: &Path) -> Custody {
        let entries = self.per_checkout.get(checkout).cloned().unwrap_or_default();
        let unknown: Vec<String> = entries
            .unknown
            .iter()
            .chain(&self.unattributed)
            .cloned()
            .collect();
        if !unknown.is_empty() {
            Custody::Unknown(unknown.join("; "))
        } else if !entries.held.is_empty() {
            Custody::Held(entries.held.join("; "))
        } else {
            Custody::Closed
        }
    }

    /// Read one pool. `Some` is the pool's own owned-outputs fact when its
    /// control state was read; `None` when the pool was already recorded as
    /// unknown, so a coordinated pair can never be assembled from it.
    fn read_pool(
        &mut self,
        pool: &Path,
        deadline: Deadline,
        state_cap: usize,
        ledger_cap: usize,
    ) -> Option<PoolFence> {
        if deadline.expired() {
            self.pool_unknown(pool, "the read deadline expired before the pool was read");
            return None;
        }
        // A control state beside the ledger is a layout this reader does not read.
        // Its presence is unknown, never a reason to read the pool as empty.
        match identity(&pool.join(STATE_FILE)) {
            Ok(None) => {}
            Ok(Some(_)) => {
                self.pool_unknown(
                    pool,
                    "a control state sits beside the ledger, outside oxide-managed, and is not read",
                );
                return None;
            }
            Err(error) => {
                self.pool_unknown(pool, &error);
                return None;
            }
        }
        let control = pool.join(CONTROL_DIR);
        let state = read_document(&control.join(STATE_FILE), state_cap, deadline)
            .and_then(|bytes| bytes.map(|bytes| parse_state(&bytes)).transpose());
        let ledger = read_document(&pool.join(LEDGER_FILE), ledger_cap, deadline)
            .and_then(|bytes| bytes.map(|bytes| parse_ledger(&bytes)).transpose());
        match (state, ledger) {
            (Ok(state), Ok(ledger)) => {
                let fact = state.as_ref().map(|state| PoolFence::of(pool, state));
                self.classify(pool, &control, state, ledger);
                fact
            }
            (state, ledger) => {
                for error in [state.err(), ledger.err()].into_iter().flatten() {
                    self.pool_unknown(pool, &error);
                }
                None
            }
        }
    }

    /// Classify one pool from its two documents. Either may be absent: an absent
    /// control state is a pool no front-door command has recorded, and an absent
    /// ledger is only consistent with an absent control state.
    fn classify(
        &mut self,
        pool: &Path,
        control: &Path,
        state: Option<RawState>,
        ledger: Option<RawLedger>,
    ) {
        let records: &[RawRecord] = state
            .as_ref()
            .map(|state| state.records.as_slice())
            .unwrap_or_default();
        let mut by_project: BTreeMap<PathBuf, &RawRecord> = BTreeMap::new();
        let mut record_ids = BTreeSet::new();
        for record in records {
            #[cfg(test)]
            crate::tape::custody_record(
                "pool_custody::classify",
                pool,
                &record.id.to_string(),
                &format!(
                    "{:?}{}",
                    record.phase,
                    if record.checkout.is_some() {
                        ""
                    } else {
                        ":no-checkout"
                    }
                ),
            );
            if !record_ids.insert(record.id) {
                self.pool_unknown(pool, &format!("record id {} appears twice", record.id));
                return;
            }
            by_project.insert(
                control.join(AFFINITY_DIR).join(record.id.to_string()),
                record,
            );
        }
        if state.is_some() && ledger.is_none() {
            self.pool_unknown(
                pool,
                "the pool ledger is missing while its control state exists",
            );
        }

        for record in records {
            let Some(checkout) = record.checkout.as_deref() else {
                // No checkout to assign to, so an active or unclosed fact is
                // pool-wide: it could be any checkout's writer.
                if let Some(detail) = unassigned_fact(record) {
                    self.pool_unknown(pool, &detail);
                }
                continue;
            };
            match record.phase {
                Phase::Idle => {}
                Phase::Acquiring | Phase::Spawning | Phase::Running => self.add_held(
                    checkout,
                    format!("slot record {} is {:?}", record.id, record.phase),
                ),
                Phase::Retired => match &record.lost {
                    None => self.add_unknown(
                        checkout,
                        format!(
                            "slot record {} is retired without a lost fact, so its writers are not established closed",
                            record.id
                        ),
                    ),
                    Some(lost) => {
                        if let Some(why) = &lost.unclosed {
                            self.add_unknown(
                                checkout,
                                format!(
                                    "slot {} is lost with writers never confirmed closed ({why})",
                                    lost.slot_id
                                ),
                            );
                        }
                    }
                },
            }
        }

        if let Some(state) = &state {
            for clean in &state.cleans {
                self.add_held(
                    &clean.checkout,
                    format!(
                        "an oxide clean for this checkout is running (mark {})",
                        clean.id
                    ),
                );
            }
        }

        let Some(ledger) = &ledger else {
            return;
        };
        let mut slot_ids = BTreeSet::new();
        for slot in &ledger.slots {
            if !slot_ids.insert(slot.id) {
                self.pool_unknown(pool, &format!("ledger slot {} appears twice", slot.id));
                return;
            }
            if slot.state == SlotState::Idle {
                continue;
            }
            let Some(record) = by_project.get(&slot.project) else {
                self.pool_unknown(
                    pool,
                    &format!(
                        "ledger slot {} is {:?} with no front-door record for its project",
                        slot.id, slot.state
                    ),
                );
                continue;
            };
            let mismatch = match (slot.state, record.phase) {
                (SlotState::Held, Phase::Idle) => Some(format!(
                    "ledger slot {} is held but its record is idle",
                    slot.id
                )),
                (SlotState::Evicting, phase) if phase != Phase::Idle => Some(format!(
                    "ledger slot {} is evicting while its record is {phase:?}",
                    slot.id
                )),
                _ => None,
            };
            if let Some(detail) = mismatch {
                match record.checkout.as_deref() {
                    Some(checkout) => self.add_unknown(checkout, detail),
                    None => self.pool_unknown(pool, &detail),
                }
            }
        }
    }

    fn add_held(&mut self, checkout: &Path, detail: String) {
        self.per_checkout
            .entry(checkout.to_path_buf())
            .or_default()
            .held
            .push(detail);
    }

    fn add_unknown(&mut self, checkout: &Path, detail: String) {
        self.per_checkout
            .entry(checkout.to_path_buf())
            .or_default()
            .unknown
            .push(detail);
    }

    fn pool_unknown(&mut self, pool: &Path, detail: &str) {
        self.unattributed
            .push(format!("{}: {detail}", pool.display()));
    }
}

/// Read one document under its byte cap and prove it did not change while it was
/// read. `Ok(None)` is absence, which is a fact. A symbolic link, a non-file, an
/// oversized or unreadable document, or one whose identity moved during the read,
/// is an error that keeps the pool.
fn read_document(path: &Path, cap: usize, deadline: Deadline) -> Result<Option<Vec<u8>>, String> {
    let before = identity(path)?;
    #[cfg(test)]
    crate::tape::stat(
        "pool_custody::read_document",
        path,
        before.map(|observed| (observed.device, observed.inode, "document")),
    );
    let bytes = match state::read_bounded(path, &Bounds::new(cap, MAX_RECORDS, deadline)) {
        #[cfg(not(test))]
        ReadOutcome::Absent => None,
        #[cfg(test)]
        ReadOutcome::Absent => {
            crate::tape::read_absent("pool_custody::read_document", path);
            None
        }
        #[cfg(not(test))]
        ReadOutcome::Present(bytes) => Some(bytes),
        #[cfg(test)]
        ReadOutcome::Present(bytes) => {
            crate::tape::read_present(
                "pool_custody::read_document",
                path,
                None,
                &bytes,
                Some(cap as u64),
            );
            Some(bytes)
        }
        #[cfg(not(test))]
        ReadOutcome::Refused(error) => return Err(error.message().to_owned()),
        #[cfg(test)]
        ReadOutcome::Refused(error) => {
            crate::tape::read_refused("pool_custody::read_document", path, error.kind().as_str());
            return Err(error.message().to_owned());
        }
    };
    #[cfg(not(test))]
    if identity(path)? != before {
        return Err(format!("{} changed while it was read", path.display()));
    }
    #[cfg(test)]
    {
        let after = identity(path)?;
        crate::tape::stat(
            "pool_custody::read_document",
            path,
            after.map(|observed| (observed.device, observed.inode, "document")),
        );
        if after != before {
            return Err(format!("{} changed while it was read", path.display()));
        }
    }
    Ok(bytes)
}

/// What a rewrite of a document changes: the file it names, its size and its
/// times. An atomic replace changes the inode; an in-place write changes size or
/// times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn identity(path: &Path) -> Result<Option<Identity>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} cannot be inspected: {error}", path.display())),
    }
}

/// Whether a fence has the value this reader knows for a state of `version`.
/// Absent is correct only below the version that introduced it.
fn fence_matches(value: Option<&str>, known: &str, first_version: u32, version: u32) -> bool {
    match value {
        None => version < first_version,
        Some(found) => found == known && version >= first_version,
    }
}

/// The unknown fact a record with no checkout still carries, if any. An idle
/// record carries none. A lost slot whose writers were confirmed closed carries
/// none. Every other active or unclosed record is a fact no checkout can own.
fn unassigned_fact(record: &RawRecord) -> Option<String> {
    match record.phase {
        Phase::Idle => None,
        Phase::Acquiring | Phase::Spawning | Phase::Running => Some(format!(
            "slot record {} is {:?} with no checkout, so its writers cannot be assigned",
            record.id, record.phase
        )),
        Phase::Retired => match &record.lost {
            None => Some(format!(
                "slot record {} is retired without a lost fact and no checkout, so its writers cannot be assigned",
                record.id
            )),
            Some(lost) => lost.unclosed.as_ref().map(|why| {
                format!(
                    "slot {} is lost with writers never confirmed closed ({why}) and no checkout",
                    lost.slot_id
                )
            }),
        },
    }
}

fn parse_state(bytes: &[u8]) -> Result<RawState, String> {
    let raw: RawState = serde_json::from_slice(bytes)
        .map_err(|error| format!("the control state is not the field set Oxide writes: {error}"))?;
    let version = raw.version;
    if !STATE_VERSIONS.contains(&version) {
        return Err(format!(
            "the control state states version {version}, which Barge does not read"
        ));
    }
    if let Some(fence) = raw.owned_outputs.as_deref() {
        if !OWNED_OUTPUTS_FENCES.contains(&fence) {
            return Err(format!(
                "the control state has an unknown owned-outputs fence {fence:?}"
            ));
        }
        // The coordinated fence is a completed paired handoff: Oxide writes it
        // with its initialization record and only records `ready` after the
        // registry image and its directory are durable. An absent, null,
        // initializing, malformed or extra-shaped record is not that handoff.
        if fence == COORDINATED_OWNED_OUTPUTS_FENCE {
            match &raw.outputs_init {
                Some(RawOutputsInit::Ready { .. }) => {}
                Some(RawOutputsInit::Initializing { .. }) => {
                    return Err(format!(
                        "the control state carries {COORDINATED_OWNED_OUTPUTS_FENCE} with an initialization that is not ready"
                    ))
                }
                None => {
                    return Err(format!(
                        "the control state carries {COORDINATED_OWNED_OUTPUTS_FENCE} without the paired initialization it always travels with"
                    ))
                }
            }
        }
    }
    // The producer bounds the initialization transaction on either variant, so
    // an unusable one is its document, not this reader's.
    if raw.outputs_init.as_ref().is_some_and(|init| {
        init.transaction().is_empty() || init.transaction().len() > INIT_TRANSACTION_CAP_BYTES
    }) {
        return Err(
            "the control state's outputs-initialization transaction is not bounded text".to_owned(),
        );
    }
    let fences_ok = fence_matches(
        raw.recorded_accounting.as_deref(),
        RECORDED_ACCOUNTING_FENCE,
        5,
        version,
    ) && fence_matches(
        raw.eviction_accounting.as_deref(),
        EVICTION_ACCOUNTING_FENCE,
        7,
        version,
    ) && fence_matches(raw.slot_ceiling.as_deref(), SLOT_CEILING_FENCE, 8, version)
        && fence_matches(raw.holding.as_deref(), HOLDING_FENCE, 9, version);
    if !fences_ok {
        return Err(format!(
            "the control state's fences do not match its version {version}"
        ));
    }
    // Version 6 is the one-use version: it must carry the fence. A class-bearing
    // record must travel under it.
    let one_use_ok = match raw.one_use_workspaces.as_deref() {
        None => version != 6,
        Some(fence) => fence == ONE_USE_FENCE && version >= 6,
    };
    let class_bearing = raw
        .records
        .iter()
        .any(|record| record.class.as_deref() == Some("one_use"));
    let classes_ok = raw
        .records
        .iter()
        .all(|record| matches!(record.class.as_deref(), None | Some("normal" | "one_use")));
    if !one_use_ok || (class_bearing && raw.one_use_workspaces.is_none()) || !classes_ok {
        return Err("the control state's one-use fence does not match its records".to_owned());
    }
    if raw.records.len() > MAX_RECORDS {
        return Err(format!(
            "the control state holds {} records, over Oxide's {MAX_RECORDS}-record ceiling",
            raw.records.len()
        ));
    }
    if raw.tickets.len() > MAX_TICKETS
        || raw.cleans.len() > MAX_CLEANS
        || raw.recoveries.len() > MAX_RECOVERIES
        || raw.flushes.len() > MAX_FLUSHES
    {
        return Err("the control state exceeds one of Oxide's list ceilings".to_owned());
    }
    Ok(raw)
}

fn parse_ledger(bytes: &[u8]) -> Result<RawLedger, String> {
    let raw: RawLedger = serde_json::from_slice(bytes)
        .map_err(|error| format!("the pool ledger is not the field set Oxide writes: {error}"))?;
    if raw.version != LEDGER_VERSION {
        return Err(format!(
            "the pool ledger states version {}, which Barge does not read",
            raw.version
        ));
    }
    if let Some(policy) = raw.capacity_policy {
        if !CAPACITY_POLICIES.contains(&policy) {
            return Err(format!(
                "the pool ledger states capacity policy {policy}, which Barge does not read"
            ));
        }
    }
    if raw.slots.len() > MAX_SLOTS {
        return Err(format!(
            "the pool ledger holds {} slots, over Oxide's {MAX_SLOTS}-slot ceiling",
            raw.slots.len()
        ));
    }
    Ok(raw)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    version: u32,
    #[serde(rename = "next_id")]
    _next_id: IgnoredAny,
    #[serde(rename = "next_use")]
    _next_use: IgnoredAny,
    tickets: Vec<IgnoredAny>,
    #[serde(default)]
    cleans: Vec<RawClean>,
    records: Vec<RawRecord>,
    #[serde(default)]
    recoveries: Vec<IgnoredAny>,
    #[serde(default)]
    flushes: Vec<IgnoredAny>,
    #[serde(default)]
    owned_outputs: Option<String>,
    /// Oxide's durable outputs-initialization record. Its deciding fact is
    /// read strictly: `/5` requires the completed `ready` variant.
    #[serde(default)]
    outputs_init: Option<RawOutputsInit>,
    #[serde(default)]
    recorded_accounting: Option<String>,
    #[serde(default)]
    one_use_workspaces: Option<String>,
    #[serde(default)]
    eviction_accounting: Option<String>,
    #[serde(default)]
    slot_ceiling: Option<String>,
    #[serde(default)]
    holding: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClean {
    id: u64,
    checkout: PathBuf,
    #[serde(default, rename = "process_group")]
    _process_group: Option<IgnoredAny>,
}

/// Oxide's outputs-initialization record, exactly the field set it writes
/// (`serde` snake_case, `deny_unknown_fields`): `initializing` is durable
/// before either control is fenced, and `ready` only after the coordinated
/// registry image and its directory are durable.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawOutputsInit {
    Initializing {
        transaction: String,
        #[allow(dead_code)]
        started_ms: u64,
    },
    Ready {
        transaction: String,
        #[allow(dead_code)]
        ready_ms: u64,
    },
}

impl RawOutputsInit {
    /// The transaction this record names, ready or not.
    fn transaction(&self) -> &str {
        match self {
            Self::Initializing { transaction, .. } | Self::Ready { transaction, .. } => transaction,
        }
    }

    /// The transaction this record names, once initialization completed.
    fn ready_transaction(&self) -> Option<&str> {
        match self {
            Self::Initializing { .. } => None,
            Self::Ready { transaction, .. } => Some(transaction),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Idle,
    Acquiring,
    Spawning,
    Running,
    Retired,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecord {
    id: u64,
    #[serde(rename = "identity")]
    _identity: IgnoredAny,
    #[serde(rename = "compatibility")]
    _compatibility: IgnoredAny,
    #[serde(rename = "profile")]
    _profile: IgnoredAny,
    #[serde(default)]
    checkout: Option<PathBuf>,
    phase: Phase,
    #[serde(default, rename = "process_group")]
    _process_group: Option<IgnoredAny>,
    #[serde(rename = "last_used")]
    _last_used: IgnoredAny,
    #[serde(default, rename = "holder")]
    _holder: Option<IgnoredAny>,
    #[serde(default, rename = "lease_id")]
    _lease_id: Option<IgnoredAny>,
    #[serde(default, rename = "pending_recovery")]
    _pending_recovery: Option<IgnoredAny>,
    #[serde(default, rename = "retired")]
    _retired: Option<IgnoredAny>,
    #[serde(default)]
    lost: Option<RawLost>,
    #[serde(default, rename = "last_used_ms")]
    _last_used_ms: Option<IgnoredAny>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default, rename = "class_reason")]
    _class_reason: Option<IgnoredAny>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLost {
    slot_id: String,
    #[serde(rename = "at_ms")]
    _at_ms: IgnoredAny,
    #[serde(default)]
    unclosed: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLedger {
    version: u32,
    #[serde(rename = "store_id")]
    _store_id: IgnoredAny,
    #[serde(rename = "cap_bytes")]
    _cap_bytes: IgnoredAny,
    #[serde(rename = "next_slot")]
    _next_slot: IgnoredAny,
    #[serde(rename = "next_lease")]
    _next_lease: IgnoredAny,
    #[serde(default)]
    capacity_policy: Option<u32>,
    #[serde(default, rename = "next_rank")]
    _next_rank: Option<IgnoredAny>,
    #[serde(default, rename = "holding")]
    _holding: Vec<IgnoredAny>,
    slots: Vec<RawSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SlotState {
    Held,
    Idle,
    Evicting,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    id: u64,
    #[serde(rename = "incarnation")]
    _incarnation: IgnoredAny,
    state: SlotState,
    project: PathBuf,
    #[serde(rename = "workspace")]
    _workspace: IgnoredAny,
    #[serde(rename = "profile")]
    _profile: IgnoredAny,
    #[serde(rename = "holder")]
    _holder: IgnoredAny,
    #[serde(default, rename = "lease_id")]
    _lease_id: Option<IgnoredAny>,
    #[serde(rename = "last_lease_id")]
    _last_lease_id: IgnoredAny,
    #[serde(default, rename = "dir")]
    _dir: Option<IgnoredAny>,
    #[serde(default, rename = "rank")]
    _rank: Option<IgnoredAny>,
    #[serde(default, rename = "recorded")]
    _recorded: Option<IgnoredAny>,
    #[serde(default, rename = "capture")]
    _capture: Option<IgnoredAny>,
    #[serde(default, rename = "hold_intent")]
    _hold_intent: Option<IgnoredAny>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use std::time::Duration;

    const CHECKOUT: &str = "/work/app";
    const OTHER: &str = "/work/other";
    /// One coordinated adoption transaction, as Oxide's 32-hex nonce spells it.
    const TRANSACTION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    /// A different transaction, for the unequal-pair case.
    const OTHER_TRANSACTION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A fresh pool directory with its control directory, under a self-removing
    /// fixture. Nothing outside the fixture is written.
    fn fixture(label: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new(label);
        let pool = dir.path().join("pool");
        std::fs::create_dir_all(pool.join(CONTROL_DIR)).unwrap();
        (dir, pool)
    }

    fn write(pool: &Path, state: Option<&str>, ledger: Option<&str>) {
        if let Some(state) = state {
            std::fs::write(pool.join(CONTROL_DIR).join(STATE_FILE), state).unwrap();
        }
        if let Some(ledger) = ledger {
            std::fs::write(pool.join(LEDGER_FILE), ledger).unwrap();
        }
    }

    fn read(pool: &Path) -> PoolCustody {
        PoolCustody::read(
            &[pool.to_path_buf()],
            Deadline::after(Duration::from_secs(10)),
        )
    }

    /// A version-9 control state with every fence Oxide writes at that version.
    fn state(records: &[String], cleans: &str) -> String {
        format!(
            r#"{{"version":9,"next_id":9,"next_use":1,"tickets":[],"cleans":[{cleans}],"records":[{}],"recorded_accounting":"oxide.accounting/1","eviction_accounting":"oxide.eviction-accounting/1","slot_ceiling":"oxide.slots/256","holding":"oxide.holding/1"}}"#,
            records.join(",")
        )
    }

    fn record(id: u64, checkout: Option<&str>, phase: &str, extra: &str) -> String {
        let checkout = checkout.map_or("null".to_owned(), |path| format!("\"{path}\""));
        format!(
            r#"{{"id":{id},"identity":"i","compatibility":"c","profile":"release","checkout":{checkout},"phase":"{phase}","process_group":null,"last_used":1{extra}}}"#
        )
    }

    fn lost(slot: &str, unclosed: Option<&str>) -> String {
        let unclosed = unclosed.map_or(String::new(), |why| format!(r#","unclosed":"{why}""#));
        format!(r#","lost":{{"slot_id":"{slot}","at_ms":1{unclosed}}}"#)
    }

    fn ledger(slots: &[String]) -> String {
        format!(
            r#"{{"version":1,"store_id":"0123456789abcdef0123456789abcdef","cap_bytes":1024,"next_slot":9,"next_lease":9,"slots":[{}]}}"#,
            slots.join(",")
        )
    }

    fn slot(id: u64, state: &str, project: &Path) -> String {
        format!(
            r#"{{"id":{id},"incarnation":1,"state":"{state}","project":"{}","workspace":"/work/app","profile":"release","holder":"h","lease_id":null,"last_lease_id":"l"}}"#,
            project.display()
        )
    }

    fn affinity(pool: &Path, id: u64) -> PathBuf {
        pool.join(CONTROL_DIR)
            .join(AFFINITY_DIR)
            .join(id.to_string())
    }

    fn checkout() -> &'static Path {
        Path::new(CHECKOUT)
    }

    /// Every path under `root` with its bytes, for proving a read changed nothing.
    fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut entries = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path.clone());
                    entries.push((path, None));
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    entries.push((path, Some(bytes)));
                }
            }
        }
        entries.sort();
        entries
    }

    fn assert_closed(reading: &PoolCustody, path: &Path) {
        assert_eq!(
            reading.for_checkout(path),
            Custody::Closed,
            "{}",
            path.display()
        );
    }

    fn assert_held(reading: &PoolCustody, path: &Path) {
        assert!(
            matches!(reading.for_checkout(path), Custody::Held(_)),
            "expected held for {}, got {:?}",
            path.display(),
            reading.for_checkout(path)
        );
    }

    fn assert_unknown(reading: &PoolCustody, path: &Path) {
        assert!(
            matches!(reading.for_checkout(path), Custody::Unknown(_)),
            "expected unknown for {}, got {:?}",
            path.display(),
            reading.for_checkout(path)
        );
    }

    fn assert_pool_unknown(reading: &PoolCustody) {
        assert!(
            reading.unattributed().is_some(),
            "expected a pool-wide unknown, got none"
        );
    }

    #[test]
    fn explicit_route_fence_preserves_numeric_version_and_legacy_coverage() {
        let (fixture, pool) = fixture("routed-output-fence");
        for (fence, covered) in [
            ("oxide.outputs/1", false),
            ("oxide.outputs/2", false),
            ("oxide.outputs/3", true),
            ("oxide.outputs/4", true),
        ] {
            let mut document: serde_json::Value =
                serde_json::from_str(&state(&[], "")).expect("state encodes");
            document["owned_outputs"] = serde_json::json!(fence);
            let bytes = serde_json::to_vec(&document).expect("state encodes");
            assert!(parse_state(&bytes).is_ok());
            std::fs::write(pool.join(CONTROL_DIR).join(STATE_FILE), bytes)
                .expect("fixture state writes");
            let reading = owned_outputs_fence(
                std::slice::from_ref(&pool),
                Deadline::after(Duration::from_secs(10)),
            );
            assert_eq!(matches!(reading, OutputsFence::Covered { .. }), covered);
        }
        let mut unknown: serde_json::Value =
            serde_json::from_str(&state(&[], "")).expect("state encodes");
        unknown["owned_outputs"] = serde_json::json!("oxide.outputs/5");
        assert!(parse_state(&serde_json::to_vec(&unknown).expect("state encodes")).is_err());
        std::fs::write(
            pool.join(CONTROL_DIR).join(STATE_FILE),
            serde_json::to_vec(&unknown).expect("state encodes"),
        )
        .expect("fixture state writes");
        assert!(matches!(
            owned_outputs_fence(&[pool], Deadline::after(Duration::from_secs(10))),
            OutputsFence::Unknown { .. }
        ));
        drop(fixture);
    }

    /// A version-9 control state carrying the coordinated fence and the raw
    /// initialization member(s) a case states, exactly as the producer writes
    /// them.
    fn coordinated_state(init: &str) -> String {
        format!(
            r#"{{"version":9,"next_id":9,"next_use":1,"tickets":[],"cleans":[],"records":[],"owned_outputs":"{COORDINATED_OWNED_OUTPUTS_FENCE}"{init},"recorded_accounting":"oxide.accounting/1","eviction_accounting":"oxide.eviction-accounting/1","slot_ceiling":"oxide.slots/256","holding":"oxide.holding/1"}}"#
        )
    }

    /// The completed initialization member, exactly Oxide's `ready` shape.
    fn ready(transaction: &str) -> String {
        format!(r#","outputs_init":{{"ready":{{"transaction":"{transaction}","ready_ms":1}}}} "#)
            .trim_end()
            .to_owned()
    }

    /// The mid-adoption initialization member, exactly Oxide's shape.
    fn initializing(transaction: &str) -> String {
        format!(
            r#","outputs_init":{{"initializing":{{"transaction":"{transaction}","started_ms":1}}}} "#
        )
        .trim_end()
        .to_owned()
    }

    /// Two pool directories: the internal pool and the removable pool inside
    /// its control directory, exactly where Oxide's writer sees them.
    fn pair(label: &str) -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new(label);
        let internal = dir.path().join("pool");
        std::fs::create_dir_all(internal.join(CONTROL_DIR)).unwrap();
        let removable = internal.join(CONTROL_DIR).join("removable");
        std::fs::create_dir_all(removable.join(CONTROL_DIR)).unwrap();
        (dir, internal, removable)
    }

    fn coordinated_read(pools: &[PathBuf]) -> OutputsFence {
        owned_outputs_fence(pools, Deadline::after(Duration::from_secs(10)))
    }

    fn coordinated_adoption(reading: &OutputsFence) -> Option<&str> {
        match reading {
            OutputsFence::Covered {
                coordinated: Some(adoption),
            } => Some(adoption.transaction()),
            _ => None,
        }
    }

    #[test]
    fn the_coordinated_fence_is_one_completed_paired_handoff() {
        // The actual emitted adoption: both controls record `/5` ready under
        // one transaction, and the custody reading reports that transaction
        // while keeping the checkout closed on the records alone.
        let (_dir, internal, removable) = pair("coordinated-pair");
        for pool in [&internal, &removable] {
            write(
                pool,
                Some(&coordinated_state(&ready(TRANSACTION))),
                Some(&ledger(&[])),
            );
        }
        let pools = [internal.clone(), removable.clone()];
        let fence = coordinated_read(&pools);
        assert_eq!(coordinated_adoption(&fence), Some(TRANSACTION), "{fence:?}");
        let both = PoolCustody::read(&pools, Deadline::after(Duration::from_secs(10)));
        assert_eq!(both.unattributed(), None, "{both:?}");
        assert_eq!(
            both.coordinated().map(CoordinatedAdoption::transaction),
            Some(TRANSACTION)
        );
        assert_closed(&both, checkout());

        // A single pool with no removable peer is the same complete pair: the
        // producer has no second control to fence.
        let (_single_dir, single, _unused) = pair("coordinated-single");
        write(
            &single,
            Some(&coordinated_state(&ready(TRANSACTION))),
            Some(&ledger(&[])),
        );
        let fence = coordinated_read(std::slice::from_ref(&single));
        assert_eq!(coordinated_adoption(&fence), Some(TRANSACTION), "{fence:?}");
    }

    #[test]
    fn a_one_sided_or_contradictory_coordinated_pair_is_unknown() {
        // The internal control adopted `/5` while the removable control still
        // says `/4`: mid-adoption, never a covered pair.
        let (_dir, internal, removable) = pair("coordinated-mixed");
        write(
            &internal,
            Some(&coordinated_state(&ready(TRANSACTION))),
            Some(&ledger(&[])),
        );
        write(
            &removable,
            Some(
                &coordinated_state("")
                    .replace(COORDINATED_OWNED_OUTPUTS_FENCE, ROUTED_OWNED_OUTPUTS_FENCE),
            ),
            Some(&ledger(&[])),
        );
        let pools = [internal.clone(), removable.clone()];
        assert!(matches!(
            coordinated_read(&pools),
            OutputsFence::Unknown { .. }
        ));
        let both = PoolCustody::read(&pools, Deadline::after(Duration::from_secs(10)));
        assert_pool_unknown(&both);
        assert_eq!(both.coordinated(), None);

        // Both controls carry `/5`, but under two different transactions: no
        // one adoption is established.
        let (_dir2, internal2, removable2) = pair("coordinated-unequal");
        write(
            &internal2,
            Some(&coordinated_state(&ready(TRANSACTION))),
            Some(&ledger(&[])),
        );
        write(
            &removable2,
            Some(&coordinated_state(&ready(OTHER_TRANSACTION))),
            Some(&ledger(&[])),
        );
        let pools2 = [internal2, removable2];
        assert!(matches!(
            coordinated_read(&pools2),
            OutputsFence::Unknown { .. }
        ));
        assert_pool_unknown(&PoolCustody::read(
            &pools2,
            Deadline::after(Duration::from_secs(10)),
        ));

        // One control adopted `/5` and the peer has no control document at all
        // (ready-only partial adoption on the other side).
        let (_dir3, internal3, removable3) = pair("coordinated-absent-peer");
        write(
            &internal3,
            Some(&coordinated_state(&ready(TRANSACTION))),
            Some(&ledger(&[])),
        );
        let pools3 = [internal3, removable3];
        assert!(matches!(
            coordinated_read(&pools3),
            OutputsFence::Unknown { .. }
        ));
        assert_pool_unknown(&PoolCustody::read(
            &pools3,
            Deadline::after(Duration::from_secs(10)),
        ));
    }

    #[test]
    fn the_coordinated_fence_requires_a_completed_bounded_initialization() {
        // Every shape that is not the producer's completed record is an
        // unknown document, never a clear or a legacy one.
        let (fixture, pool) = fixture("coordinated-init-shapes");
        for (label, init) in [
            ("absent", String::new()),
            ("null", r#","outputs_init":null"#.to_owned()),
            ("initializing", initializing(TRANSACTION)),
            (
                "empty-transaction",
                r#","outputs_init":{"ready":{"transaction":"","ready_ms":1}}"#.to_owned(),
            ),
            (
                "missing-time",
                format!(
                    r#","outputs_init":{{"ready":{{"transaction":"{TRANSACTION}"}}}}"#
                ),
            ),
            (
                "extra-member",
                format!(
                    r#","outputs_init":{{"ready":{{"transaction":"{TRANSACTION}","ready_ms":1,"extra":true}}}}"#
                ),
            ),
            (
                "unknown-variant",
                format!(
                    r#","outputs_init":{{"done":{{"transaction":"{TRANSACTION}","ready_ms":1}}}}"#
                ),
            ),
            (
                "duplicate",
                format!(
                    r#","outputs_init":{{"ready":{{"transaction":"{TRANSACTION}","ready_ms":1}}}},&#10;"outputs_init":{{"ready":{{"transaction":"{TRANSACTION}","ready_ms":1}}}}"#
                )
                .replace("&#10;", ""),
            ),
        ] {
            let document = coordinated_state(&init);
            assert!(
                parse_state(document.as_bytes()).is_err(),
                "{label} read as a completed initialization"
            );
            std::fs::write(pool.join(CONTROL_DIR).join(STATE_FILE), &document)
                .expect("fixture state writes");
            let reading = coordinated_read(std::slice::from_ref(&pool));
            assert!(
                matches!(reading, OutputsFence::Unknown { .. }),
                "{label} fence read as {reading:?}"
            );
            assert_pool_unknown(&read(&pool));
        }

        // An unknown future fence and a coordinated fence below its numeric
        // floor stay unknown too.
        std::fs::write(
            pool.join(CONTROL_DIR).join(STATE_FILE),
            coordinated_state(&ready(TRANSACTION))
                .replace(COORDINATED_OWNED_OUTPUTS_FENCE, "oxide.outputs/6"),
        )
        .expect("fixture state writes");
        assert!(matches!(
            coordinated_read(std::slice::from_ref(&pool)),
            OutputsFence::Unknown { .. }
        ));
        std::fs::write(
            pool.join(CONTROL_DIR).join(STATE_FILE),
            format!(
                r#"{{"version":3,"next_id":9,"next_use":1,"tickets":[],"records":[],"owned_outputs":"{COORDINATED_OWNED_OUTPUTS_FENCE}"{}}}"#,
                ready(TRANSACTION)
            ),
        )
        .expect("fixture state writes");
        assert!(matches!(
            coordinated_read(std::slice::from_ref(&pool)),
            OutputsFence::Unknown { .. }
        ));
        drop(fixture);
    }

    #[test]
    fn a_pool_that_was_never_used_by_the_front_door_is_closed() {
        let (_dir, pool) = fixture("custody-never-used");
        let reading = read(&pool);
        assert_closed(&reading, checkout());
        assert_eq!(reading.unattributed(), None);
    }

    #[test]
    fn idle_records_and_idle_slots_are_closed() {
        let (_dir, pool) = fixture("custody-idle");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "idle", "")], "")),
            Some(&ledger(&[slot(1, "idle", &affinity(&pool, 1))])),
        );
        let reading = read(&pool);
        assert_closed(&reading, checkout());
        assert_eq!(reading.unattributed(), None);
    }

    #[test]
    fn a_writer_holds_only_the_checkout_its_record_serves() {
        let (_dir, pool) = fixture("custody-running");
        write(
            &pool,
            Some(&state(
                &[
                    record(1, Some(CHECKOUT), "running", ""),
                    record(2, Some(OTHER), "idle", ""),
                ],
                "",
            )),
            Some(&ledger(&[
                slot(1, "held", &affinity(&pool, 1)),
                slot(2, "idle", &affinity(&pool, 2)),
            ])),
        );
        let reading = read(&pool);
        assert_held(&reading, checkout());
        assert_closed(&reading, Path::new(OTHER));
    }

    #[test]
    fn every_phase_that_may_still_write_or_lease_holds_the_checkout() {
        for phase in ["acquiring", "spawning", "running"] {
            let (_dir, pool) = fixture("custody-phase");
            write(
                &pool,
                Some(&state(&[record(1, Some(CHECKOUT), phase, "")], "")),
                Some(&ledger(&[])),
            );
            assert_held(&read(&pool), checkout());
        }
    }

    #[test]
    fn an_active_record_with_no_checkout_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-no-checkout");
        write(
            &pool,
            Some(&state(&[record(1, None, "running", "")], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        let reading = read(&pool);
        assert_unknown(&reading, checkout());
        assert_pool_unknown(&reading);
    }

    #[test]
    fn an_idle_record_with_no_checkout_names_no_checkout() {
        let (_dir, pool) = fixture("custody-idle-no-checkout");
        write(
            &pool,
            Some(&state(&[record(1, None, "idle", "")], "")),
            Some(&ledger(&[slot(1, "idle", &affinity(&pool, 1))])),
        );
        let reading = read(&pool);
        assert_closed(&reading, checkout());
        assert_eq!(reading.unattributed(), None);
    }

    #[test]
    fn an_unclosed_lost_record_with_no_checkout_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-lost-no-checkout");
        let retired = record(1, None, "retired", &lost("3", Some("writers unknown")));
        write(
            &pool,
            Some(&state(&[retired], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_held_row_on_an_idle_record_with_no_checkout_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-held-idle-no-checkout");
        write(
            &pool,
            Some(&state(&[record(1, None, "idle", "")], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_lost_slot_with_unclosed_writers_keeps_its_checkout() {
        let (_dir, pool) = fixture("custody-lost-unclosed");
        let retired = record(
            1,
            Some(CHECKOUT),
            "retired",
            &lost("3", Some("a cargo group was never confirmed empty")),
        );
        write(
            &pool,
            Some(&state(&[retired], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        assert_unknown(&read(&pool), checkout());
    }

    #[test]
    fn a_lost_slot_whose_writers_are_closed_does_not_hold_its_checkout() {
        let (_dir, pool) = fixture("custody-lost-closed");
        let retired = record(1, Some(CHECKOUT), "retired", &lost("3", None));
        write(
            &pool,
            Some(&state(&[retired], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        let reading = read(&pool);
        assert_closed(&reading, checkout());
        assert_eq!(reading.unattributed(), None);
    }

    #[test]
    fn a_retired_record_without_a_lost_fact_is_unknown() {
        let (_dir, pool) = fixture("custody-retired-no-lost");
        let retired = record(1, Some(CHECKOUT), "retired", r#","retired":"flush failed""#);
        write(
            &pool,
            Some(&state(&[retired], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        assert_unknown(&read(&pool), checkout());
    }

    #[test]
    fn a_held_slot_on_an_idle_record_is_unknown() {
        let (_dir, pool) = fixture("custody-held-idle");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "idle", "")], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        assert_unknown(&read(&pool), checkout());
    }

    #[test]
    fn a_held_slot_with_no_record_for_its_project_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-held-orphan");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "idle", "")], "")),
            Some(&ledger(&[slot(7, "held", &affinity(&pool, 7))])),
        );
        let reading = read(&pool);
        assert_unknown(&reading, checkout());
        assert_pool_unknown(&reading);
    }

    #[test]
    fn an_evicting_slot_is_closed_on_an_idle_record_and_unknown_on_an_active_one() {
        let (_dir, pool) = fixture("custody-evicting-idle");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "idle", "")], "")),
            Some(&ledger(&[slot(1, "evicting", &affinity(&pool, 1))])),
        );
        let reading = read(&pool);
        assert_closed(&reading, checkout());
        assert_eq!(reading.unattributed(), None);

        let (_dir, pool) = fixture("custody-evicting-active");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "running", "")], "")),
            Some(&ledger(&[slot(1, "evicting", &affinity(&pool, 1))])),
        );
        assert_unknown(&read(&pool), checkout());
    }

    #[test]
    fn a_running_clean_holds_its_checkout() {
        let (_dir, pool) = fixture("custody-clean");
        let clean = format!(r#"{{"id":4,"checkout":"{CHECKOUT}","process_group":null}}"#);
        write(&pool, Some(&state(&[], &clean)), Some(&ledger(&[])));
        assert_held(&read(&pool), checkout());
    }

    #[test]
    fn unknown_beats_held_for_the_same_checkout() {
        let (_dir, pool) = fixture("custody-precedence");
        let running = record(1, Some(CHECKOUT), "running", "");
        let lost_unclosed = record(
            2,
            Some(CHECKOUT),
            "retired",
            &lost("5", Some("writers unknown")),
        );
        write(
            &pool,
            Some(&state(&[running, lost_unclosed], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        match read(&pool).for_checkout(checkout()) {
            Custody::Unknown(detail) => assert!(detail.contains("writers unknown"), "{detail}"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_ledger_beside_a_control_state_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-no-ledger");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "idle", "")], "")),
            None,
        );
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_malformed_control_state_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-malformed");
        write(&pool, Some("{not json"), Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn an_unknown_control_version_is_pool_wide_unknown() {
        for version in [0, 10] {
            let (_dir, pool) = fixture("custody-version");
            let text = state(&[record(1, Some(CHECKOUT), "idle", "")], "").replacen(
                "\"version\":9",
                &format!("\"version\":{version}"),
                1,
            );
            write(&pool, Some(&text), Some(&ledger(&[])));
            assert_pool_unknown(&read(&pool));
        }
    }

    #[test]
    fn an_unknown_field_in_a_record_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-unknown-field");
        let surprise = record(1, Some(CHECKOUT), "idle", r#","surprise":1"#);
        write(&pool, Some(&state(&[surprise], "")), Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn an_unknown_fence_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-fence");
        let text = state(&[], "").replace("oxide.holding/1", "oxide.holding/2");
        write(&pool, Some(&text), Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_fence_missing_at_its_version_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-missing-fence");
        let text = state(&[], "").replace(r#","holding":"oxide.holding/1""#, "");
        write(&pool, Some(&text), Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn an_unknown_ledger_version_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-ledger-version");
        let text = ledger(&[]).replacen("\"version\":1", "\"version\":2", 1);
        write(&pool, Some(&state(&[], "")), Some(&text));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn an_unknown_slot_state_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-slot-state");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "idle", "")], "")),
            Some(&ledger(&[slot(1, "parked", &affinity(&pool, 1))])),
        );
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn an_oversized_document_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-oversized");
        write(&pool, Some(&state(&[], "")), Some(&ledger(&[])));
        let reading = PoolCustody::read_with_caps(
            std::slice::from_ref(&pool),
            Deadline::after(Duration::from_secs(10)),
            16,
            LEDGER_CAP_BYTES,
        );
        assert_pool_unknown(&reading);
    }

    #[test]
    fn too_many_records_are_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-records");
        let records: Vec<String> = (1..=(MAX_RECORDS as u64 + 1))
            .map(|id| record(id, None, "idle", ""))
            .collect();
        write(&pool, Some(&state(&records, "")), Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_symbolic_link_document_is_pool_wide_unknown() {
        let (dir, pool) = fixture("custody-symlink");
        let real = dir.path().join("real-state.json");
        std::fs::write(&real, state(&[], "")).unwrap();
        std::os::unix::fs::symlink(&real, pool.join(CONTROL_DIR).join(STATE_FILE)).unwrap();
        write(&pool, None, Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_document_that_is_a_directory_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-directory");
        std::fs::create_dir(pool.join(CONTROL_DIR).join(STATE_FILE)).unwrap();
        write(&pool, None, Some(&ledger(&[])));
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_spent_deadline_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-deadline");
        write(&pool, Some(&state(&[], "")), Some(&ledger(&[])));
        let reading =
            PoolCustody::read(std::slice::from_ref(&pool), Deadline::after(Duration::ZERO));
        assert_pool_unknown(&reading);
    }

    #[test]
    fn no_pool_at_all_is_unknown_never_closed() {
        let reading = PoolCustody::read(&[], Deadline::after(Duration::from_secs(10)));
        assert_pool_unknown(&reading);
        assert_unknown(&reading, checkout());
    }

    #[test]
    fn a_stray_control_state_beside_the_ledger_is_pool_wide_unknown() {
        let (_dir, pool) = fixture("custody-stray-state");
        write(&pool, None, Some(&ledger(&[])));
        std::fs::write(pool.join(STATE_FILE), state(&[], "")).unwrap();
        assert_pool_unknown(&read(&pool));
    }

    #[test]
    fn a_writer_in_the_removable_pool_holds_its_checkout_across_both_pools() {
        let (dir, internal) = fixture("custody-two-pools");
        let removable = internal.join(CONTROL_DIR).join(REMOVABLE_POOL_DIR);
        std::fs::create_dir_all(removable.join(CONTROL_DIR)).unwrap();
        write(&internal, Some(&state(&[], "")), Some(&ledger(&[])));
        write(
            &removable,
            Some(&state(&[record(1, Some(CHECKOUT), "running", "")], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&removable, 1))])),
        );
        let reading = PoolCustody::read(
            &[internal.clone(), removable.clone()],
            Deadline::after(Duration::from_secs(10)),
        );
        assert_held(&reading, checkout());
        assert_eq!(reading.pools(), &[internal.clone(), removable.clone()]);
        drop(dir);
    }

    #[test]
    fn a_replaced_document_has_a_different_identity() {
        let (_dir, pool) = fixture("custody-identity");
        let path = pool.join(LEDGER_FILE);
        write(&pool, None, Some(&ledger(&[])));
        let before = identity(&path).unwrap();
        let replacement = pool.join("ledger.replacement");
        std::fs::write(&replacement, ledger(&[])).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(before, identity(&path).unwrap());
    }

    #[test]
    fn reading_creates_no_file_and_changes_nothing() {
        let (_dir, pool) = fixture("custody-no-write");
        write(
            &pool,
            Some(&state(&[record(1, Some(CHECKOUT), "running", "")], "")),
            Some(&ledger(&[slot(1, "held", &affinity(&pool, 1))])),
        );
        let before = tree(&pool);
        let _ = read(&pool);
        assert_eq!(
            tree(&pool),
            before,
            "a read must write, lock and create nothing"
        );
        assert!(!pool.join("lock").exists());
        assert!(!pool.join(CONTROL_DIR).join("lock").exists());
    }
}
