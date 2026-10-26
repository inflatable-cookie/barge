//! Writer custody: the evidence Barge can establish, the three verdicts and the
//! deletion authority only `Safe` carries (stage 1 design, "Writer custody and
//! safe deletion").
//!
//! macOS has no unprivileged primitive that proves "no process holds this
//! subtree open", so the verdict is explicitly *cooperative holds plus a bounded
//! same-uid open-descriptor observation*. `Safe` requires all of:
//!
//! - every recorded hold for the root released or positively stale;
//! - the same-uid descriptor scan complete within its bound and empty for the
//!   root, with the calling process's own descriptors excluded by pid and start
//!   identity (the walk's and the checks' handles are not another writer) and
//!   every process of another effective uid classified and reported rather than
//!   inspected;
//! - the root's device, inode and marker digest equal to the observation the
//!   caller was given;
//! - the volume still verified; and
//! - no pending intent naming another root.
//!
//! Two residuals are named in [`capability_report`] rather than implied: a
//! process of another effective uid, which the kernel's own process table
//! classifies and the evidence counts but nothing inspects, and a process that
//! dropped its hold and kept writing. A wrong delete stays recoverable through
//! the intent journal and quarantine rename. The scan excludes the calling
//! `barge` process and nothing else: another Barge process, its child or its
//! parent holds a descriptor like any other writer. `lsof` is never custody: it
//! is external, unbounded and its silence is not a kernel guarantee, so nothing
//! here reads it.
//!
//! Deletion and swap take a [`Safe`](CustodyVerdict::Safe) value, so a caller
//! cannot pass a boolean and cannot reach the effect on a refusal.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::area::{self, HoldState, MarkerExpectation, MarkerState};
use crate::contract::HOLD_ROWS;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    DescriptorScan, DescriptorScanRequest, DeviceId, DirOps, MountRoot, ProcessObserver,
    SelfExclusion, VolumeFacts, VolumeUuid,
};
use crate::registry;
use crate::state::{self, IntentRecord, StatePaths};

/// The capability this build offers, as `describe.custody.capability` states it.
pub const CAPABILITY: &str = "cooperative_holds_plus_same_uid_descriptor_observation";

/// How many same-uid processes one scan may observe before it is bounded.
pub const SCAN_PROCESSES: usize = 4096;

/// How many descriptors one scan may observe before it is bounded.
pub const SCAN_DESCRIPTORS: usize = 65_536;

/// How long one descriptor scan may take before it is bounded.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// What a caller observed about the root it asks to act on.
///
/// It is the caller's evidence, not the root's truth: custody re-reads the same
/// facts and refuses unless they still match. The marker digest is the SHA-256
/// of the marker bytes the caller read, which is what makes "the same root" mean
/// the same bytes as well as the same inode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedRoot {
    /// The root's id, which is also its directory name.
    pub root_id: String,
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The volume's `root_dir` the caller resolved the root under.
    pub root_dir: String,
    /// The area folder the caller resolved the root in.
    pub folder: String,
    /// The root directory's device.
    pub device: DeviceId,
    /// The root directory's inode on that device.
    pub inode: u64,
    /// The SHA-256 of the root's marker bytes.
    pub marker_digest: String,
    /// The absolute path the caller's walk reached.
    pub path: PathBuf,
}

impl ObservedRoot {
    /// The observation an owned root's creation returned, in this shape.
    pub fn from_owned(owned: &area::OwnedRoot, root_dir: &str, folder: &str) -> Self {
        Self {
            root_id: owned.root_id.clone(),
            volume_uuid: owned.volume_uuid.clone(),
            root_dir: root_dir.to_owned(),
            folder: folder.to_owned(),
            device: owned.identity.device,
            inode: owned.identity.inode,
            marker_digest: owned.marker_digest.clone(),
            path: owned.path.clone(),
        }
    }

    /// The observation a bounded root listing reported, when its marker was
    /// owned and its digest was read.
    pub fn from_listing(
        row: &area::RootObservation,
        volume_uuid: &VolumeUuid,
        root_dir: &str,
        folder: &str,
    ) -> Option<Self> {
        Some(Self {
            root_id: row.root_id.clone()?,
            volume_uuid: volume_uuid.clone(),
            root_dir: root_dir.to_owned(),
            folder: folder.to_owned(),
            device: DeviceId::new(row.device?),
            inode: row.inode?,
            marker_digest: row.marker_digest.clone()?,
            path: row.configured_path.clone(),
        })
    }
}

/// One pending intent, as custody needs to know it: an intent for any other root
/// means a deletion is already in flight, so this one must not start, and an
/// intent for this root means it is already mid-delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingIntent {
    /// The intent's id.
    pub intent_id: String,
    /// The root the intent acts on.
    pub root_id: String,
}

impl PendingIntent {
    /// Project the recorded intents that still need a replay decision.
    pub fn pending(records: &[IntentRecord]) -> Vec<Self> {
        records
            .iter()
            .filter(|record| record.is_pending())
            .map(|record| Self {
                intent_id: record.intent_id.clone(),
                root_id: record.root_id.clone(),
            })
            .collect()
    }
}

/// The bounds one custody decision runs within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustodyBounds {
    /// How many hold files one root may carry.
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
    /// The root the evidence is about, re-read and matched.
    pub root_id: String,
    /// The volume UUID that verified.
    pub volume_uuid: VolumeUuid,
    /// The device the volume reported.
    pub device: u64,
    /// The inode the root's descriptor reported.
    pub inode: u64,
    /// The marker digest that matched the caller's observation.
    pub marker_digest: String,
    /// How many hold files the root carried.
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
    /// When the verdict was decided.
    pub checked_ms: u64,
}

/// The three verdicts. Only [`Safe`](Self::Safe) authorises an effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustodyVerdict {
    /// Every fact Barge can establish agrees: a swap or delete may proceed.
    Safe(Box<CustodyEvidence>),
    /// A holder or an open descriptor was found. Nothing may be deleted.
    Unsafe {
        /// What was found, for a human.
        reason: String,
    },
    /// A fact could not be read or could not be completed within its bound.
    /// Unknown is a refusal and is never read as safe.
    Unknown {
        /// Why the verdict cannot be decided.
        reason: String,
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
            Self::Unsafe { reason } | Self::Unknown { reason } => Some(reason),
        }
    }

    /// The authority an effect needs, or the refusal that replaces it.
    ///
    /// This is the type-level gate: [`crate::delete`] takes the evidence a
    /// `Safe` verdict produced and nothing else.
    pub fn into_evidence(self) -> Result<CustodyEvidence> {
        match self {
            Self::Safe(evidence) => Ok(*evidence),
            Self::Unsafe { reason } | Self::Unknown { reason } => Err(Error::new(
                ErrorKind::CustodyNotProven,
                format!("custody is not proven: {reason}"),
            )),
        }
    }
}

/// Decide custody for one observed root.
///
/// Every fact is read fresh. A hold that is live or unknown, a scan that is
/// bounded or unreadable, a root whose identity or marker digest moved, a volume
/// that no longer verifies, and a pending intent for another root (or for this
/// one) all refuse. Nothing is written and nothing is deleted here.
#[allow(clippy::too_many_arguments)]
pub fn take_custody(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    observed: &ObservedRoot,
    pending: &[PendingIntent],
    bounds: CustodyBounds,
) -> CustodyVerdict {
    match decide(
        dirs,
        facts,
        mounts,
        observer,
        state_paths,
        observed,
        pending,
        bounds,
    ) {
        Ok(evidence) => CustodyVerdict::Safe(Box::new(evidence)),
        Err(Refusal::Unsafe(reason)) => CustodyVerdict::Unsafe { reason },
        Err(Refusal::Unknown(reason)) => CustodyVerdict::Unknown { reason },
    }
}

/// A non-safe verdict, before it becomes a [`CustodyVerdict`].
enum Refusal {
    Unsafe(String),
    Unknown(String),
}

#[allow(clippy::too_many_arguments)]
fn decide(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    state_paths: &StatePaths,
    observed: &ObservedRoot,
    pending: &[PendingIntent],
    bounds: CustodyBounds,
) -> std::result::Result<CustodyEvidence, Refusal> {
    // 1. Holds: released, or positively stale. Anything else is a refusal.
    let holds = area::read_holds(state_paths, observer, &observed.root_id)
        .map_err(|error| Refusal::Unknown(format!("the root's holds are unreadable: {error}")))?;
    if holds.total > bounds.holds || holds.truncated {
        return Err(Refusal::Unknown(format!(
            "the root carries {} hold files, over the {}-hold bound",
            holds.total, bounds.holds
        )));
    }
    let mut stale = 0usize;
    for (record, hold_state, reason) in &holds.holds {
        match hold_state {
            HoldState::Live => {
                return Err(Refusal::Unsafe(format!(
                    "the holder `{}` on pid {} is live",
                    record.holder, record.pid
                )))
            }
            HoldState::Unknown => {
                return Err(Refusal::Unknown(format!(
                    "the hold `{}` cannot be decided: {}",
                    record.holder,
                    reason.as_deref().unwrap_or("no reason was recorded")
                )))
            }
            HoldState::Stale => stale = stale.saturating_add(1),
        }
    }

    // 2. No pending intent may name another root, and this root must not already
    //    be mid-delete.
    if let Some(intent) = pending.first() {
        if intent.root_id == observed.root_id {
            return Err(Refusal::Unknown(format!(
                "the pending intent {} already names this root",
                intent.intent_id
            )));
        }
        return Err(Refusal::Unknown(format!(
            "the pending intent {} names root {}, so this deletion must wait",
            intent.intent_id, intent.root_id
        )));
    }

    // 3. The volume still verifies, and the root is still the inode and the
    //    marker bytes the caller observed.
    let record = registry::read(state_paths)
        .map_err(|error| Refusal::Unknown(format!("the registry is unreadable: {error}")))?
        .volume(&observed.volume_uuid)
        .cloned()
        .ok_or_else(|| {
            Refusal::Unknown(format!("volume {} is not registered", observed.volume_uuid))
        })?;
    let opened = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        &record,
        &observed.root_dir,
        &observed.folder,
    )
    .map_err(|error| Refusal::Unknown(format!("the area folder does not verify: {error}")))?;
    let Some((area_path, area_handle)) = opened else {
        return Err(Refusal::Unknown(format!(
            "the area folder for {} is not there",
            observed.root_id
        )));
    };
    let root = dirs
        .open_child_dir(&area_handle, &observed.root_id)
        .map_err(|error| Refusal::Unknown(format!("the root cannot be opened: {error}")))?;
    if root.identity().device != observed.device || root.identity().inode != observed.inode {
        return Err(Refusal::Unknown(format!(
            "the root at {} is device {} inode {}, not the observed {} / {}",
            area_path.join(&observed.root_id).display(),
            root.identity().device,
            root.identity().inode,
            observed.device,
            observed.inode
        )));
    }
    let root_path = area_path.join(&observed.root_id);
    let expectation = MarkerExpectation {
        volume_uuid: &observed.volume_uuid,
        resolved_path: &root_path,
        effective_uid: observer
            .effective_uid()
            .map_err(|error| Refusal::Unknown(format!("the account uid is unreadable: {error}")))?,
        now_ms: state::now_ms(),
    };
    let (marker, digest) = area::read_marker_with_digest(dirs, &root, &expectation);
    match marker {
        MarkerState::Owned(owned) if owned.root_id == observed.root_id => {}
        MarkerState::Owned(other) => {
            return Err(Refusal::Unknown(format!(
                "the marker names root {}, not the observed {}",
                other.root_id, observed.root_id
            )))
        }
        MarkerState::Absent => {
            return Err(Refusal::Unknown(
                "the root carries no marker, so it is not owned".to_owned(),
            ))
        }
        MarkerState::Unknown { reason } => {
            return Err(Refusal::Unknown(format!(
                "the root's marker is not owned: {reason}"
            )))
        }
    }
    let digest = digest
        .ok_or_else(|| Refusal::Unknown("the root's marker digest could not be read".to_owned()))?;
    if digest != observed.marker_digest {
        return Err(Refusal::Unknown(format!(
            "the marker at {} is digest {digest}, not the observed {}",
            root_path.display(),
            observed.marker_digest
        )));
    }

    // 4. The same-uid descriptor observation, complete within its bound.
    //
    //    The calling process holds descriptors inside the root for the walk and
    //    for the checks that follow, so its own pid is excluded from the
    //    evidence: read once here, with the start identity the kernel reports
    //    for it, and verified by the platform as it scans. Nothing else is
    //    excluded, and an identity that cannot be read or does not match is a
    //    refusal, never a skip.
    let self_pid = std::process::id();
    let self_start = observer
        .process_start(self_pid)
        .map_err(|error| {
            Refusal::Unknown(format!(
                "the calling process's start identity is unreadable: {error}"
            ))
        })?
        .ok_or_else(|| {
            Refusal::Unknown(format!(
                "the calling process pid {self_pid} has no start identity, so its own descriptors cannot be excluded"
            ))
        })?;
    let scan = observer.scan_open_descriptors(&DescriptorScanRequest {
        root: &root_path,
        root_device: observed.device,
        root_inode: observed.inode,
        max_processes: bounds.processes,
        max_descriptors: bounds.descriptors,
        timeout: bounds.timeout,
        self_exclusion: Some(SelfExclusion {
            pid: self_pid,
            start: self_start,
        }),
    });
    let (processes, descriptors, foreign) = match scan {
        Err(error) => {
            return Err(Refusal::Unknown(format!(
                "the open-descriptor scan is unreadable: {error}"
            )))
        }
        Ok(DescriptorScan::Bounded {
            processes,
            descriptors,
            bound,
        }) => {
            return Err(Refusal::Unknown(format!(
                "the open-descriptor scan reached its {bound} bound after {processes} processes and {descriptors} descriptors"
            )))
        }
        Ok(DescriptorScan::Complete {
            processes,
            descriptors,
            foreign,
            open,
        }) => {
            if let Some(first) = open.first() {
                return Err(Refusal::Unsafe(format!(
                    "pid {} holds descriptor {} open at {}",
                    first.pid,
                    first.descriptor,
                    first.path.display()
                )));
            }
            (processes, descriptors, foreign)
        }
    };

    Ok(CustodyEvidence {
        root_id: observed.root_id.clone(),
        volume_uuid: observed.volume_uuid.clone(),
        device: observed.device.get(),
        inode: observed.inode,
        marker_digest: digest,
        holds: holds.total,
        holds_stale: stale,
        processes,
        descriptors,
        self_excluded: self_pid,
        foreign_uid_processes: foreign,
        checked_ms: state::now_ms(),
    })
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
            "every recorded hold for the root is released or positively stale".to_owned(),
            "a bounded scan of this account's open descriptors is complete and finds none at or under the root".to_owned(),
            "that scan classifies each process by the effective uid the kernel's own process table reports, before it inspects any descriptor, and reports how many processes of another effective uid it classified".to_owned(),
            "that scan excludes the calling barge process's own descriptors, by its pid and the start identity read once and verified, and no other process: another barge process, its child or its parent is observed like any other same-uid writer".to_owned(),
            "the root's device, inode and marker digest equal the observation the caller was given".to_owned(),
            "the volume still verifies as the registered one".to_owned(),
            "no pending intent names another root".to_owned(),
        ],
        excludes: vec![
            "a process of another effective uid: classified from the kernel's process table and reported in the evidence as not observed, never inspected".to_owned(),
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

    use super::{
        capability_report, take_custody, CustodyBounds, CustodyVerdict, ObservedRoot,
        PendingIntent, CAPABILITY,
    };
    use crate::area::RootKind;
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::{
        platform, DescriptorScan, DescriptorScanRequest, OpenDescriptor, ProcessIdentity,
        ProcessObserver, VolumeUuid,
    };
    use crate::testkit::{Faults, ScanMode, VolumeFixture, FIXTURE_BOOT, OTHER_UUID};

    struct Holder {
        fixture: VolumeFixture,
        observed: ObservedRoot,
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
        let (root, _) = fixture.create_root(&fixture.record, &declaration, RootKind::AreaRoot);
        let observed = ObservedRoot::from_owned(&root, "", "example/pool");
        Holder { fixture, observed }
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
    /// The classification, the descriptor enumeration, the path matching and the
    /// self-exclusion are the production ones; only the table is injected, which
    /// is how an unprivileged fact about another account, an unreadable uid or a
    /// table that claims a pid this account cannot read is exercised against the
    /// real kernel reads.
    struct TableObserver {
        inner: crate::os::MacOs,
        table: Vec<ProcessIdentity>,
    }

    impl TableObserver {
        fn new(table: Vec<ProcessIdentity>) -> Self {
            Self {
                inner: platform(),
                table,
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

        fn scan_open_descriptors(
            &self,
            request: &DescriptorScanRequest<'_>,
        ) -> Result<DescriptorScan> {
            self.inner.scan_open_descriptors_over(request, &self.table)
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
                .join(&self.observed.root_id)
                .join(format!("{holder}.json"));
            std::fs::create_dir_all(path.parent().expect("the hold parent"))
                .expect("the holds dir");
            std::fs::write(
                &path,
                VolumeFixture::hold_record(
                    &self.observed.root_id,
                    holder,
                    pid,
                    process_start,
                    boot,
                ),
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
                &self.observed,
                pending,
                bounds,
            )
        }

        /// The root's absolute path, as the walk reached it.
        fn root_path(&self) -> PathBuf {
            self.observed.path.clone()
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
    }

    #[test]
    fn a_released_root_with_a_complete_empty_scan_is_safe() {
        let holder = holder("custody-safe");
        let verdict = holder.safe();
        assert_eq!(verdict.name(), "safe");
        let evidence = verdict.evidence().expect("the evidence");
        assert_eq!(evidence.root_id, holder.observed.root_id);
        assert_eq!(evidence.holds, 0);
        assert_eq!(evidence.marker_digest, holder.observed.marker_digest);
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
            .join(&holder.observed.root_id)
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
            root_id: "b".repeat(32),
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
            root_id: holder.observed.root_id.clone(),
        }];
        let verdict = holder.take(&pending, &holder.fixture.observer());
        assert_eq!(verdict.name(), "unknown");
        assert!(verdict.reason().unwrap_or_default().contains("already"));
    }

    #[test]
    fn a_marker_digest_that_moved_is_unknown() {
        let holder = holder("custody-digest");
        let mut observed = holder.observed.clone();
        observed.marker_digest = "0".repeat(64);
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
        let mut observed = holder.observed.clone();
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
        let mut observed = holder.observed.clone();
        observed.volume_uuid = VolumeUuid::parse(OTHER_UUID).expect("a UUID");
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
            &holder.observed,
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
    fn a_process_this_account_may_not_read_is_unknown_even_when_the_table_claims_it() {
        let holder = real_holder("custody-real-descriptors-unreadable");
        holder.payload();
        // A pid whose descriptors the kernel will not show this account, claimed
        // by the table as this account's own. The descriptor read refuses, and a
        // refusal to read is unknown, never "nothing open".
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
        assert_eq!(verdict.name(), "unknown", "{:?}", verdict.reason());
        assert!(verdict
            .reason()
            .unwrap_or_default()
            .contains("descriptors of pid"));
        assert!(verdict.clone().into_evidence().is_err(), "unknown refuses");
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
}
