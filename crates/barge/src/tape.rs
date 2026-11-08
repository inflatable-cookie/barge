//! The F6-O2 B-S1 observation tape: what the release planner actually read.
//!
//! This module is compiled only under `cfg(test)` (`lib.rs` declares it as a
//! `pub(crate)` `cfg(test)` module), so a production build compiles none of it
//! and the covering release-profile check is intended to prove that. Calls use
//! test-only statements or narrow test/non-test splits that retain the original
//! non-test statement. The tape is one-sided: it records what the real syscalls and
//! parsers returned and feeds nothing back — no policy is copied, no custody,
//! absence or eligible verdict is injected, and no fixture fact is invented.
//!
//! ## Transcript
//!
//! [`finish`] serializes the window as the recorder's own
//! `barge.capture.recorder/1` diagnostic document, always written, including
//! when the planner refused: the caps, the per-row provenance flag, the
//! fixture capture, the notes and the engine's own result or refusal.
//! [`shared_transcript`] then converts it to the reviewed
//! `oxide.capture.transcript/2` capture, and [`assess_shared`] judges that
//! transcript with the copied shared completeness rules. Identity fields
//! contain only available observations. Rows without audited identity binding
//! remain unproven, and the window remains incomplete. An outcome is a bytes
//! digest, an explicit absent, a listed count, or a refusal class — never a
//! defaulted verdict. An apparatus refusal or a fallback is the distinct
//! `oxide.capture.apparatus/2` diagnostic, never a capture.
//!
//! ## Caps (fixed by the accepted capture contract)
//!
//! 8192 read rows, 8 MiB of document, at most 64 occurrences per identity key
//! and 512 process rows. An exhausted cap is recorded as truncation and never
//! read as a complete window.
//!
//! ## Epochs and occurrences
//!
//! Epochs are engine-local, monotonic, assigned at the leaf in row order.
//! Occurrences are counted per identity key within the window, so a root-lock
//! marker reread appears as a second bound occurrence of the same key.
//!
//! ## Recorded inventory (B-S1 leaf table)
//!
//! - `os/macos.rs`: the kernel process table and each of its rows, the per-fd
//!   `PROC_PIDLISTFDS` listings, the per-descriptor `PROC_PIDFDVNODEPATHINFO`
//!   and `PROC_PIDFDVNODEINFO` answers (including every bounded retry), the
//!   `csops` code-signing status and its verdicts, the effective uid and boot
//!   identity, per-pid start identities, the mount census and mount-root
//!   proofs, the volume identity reads, and the `DirOps` leaves: opens, child
//!   facts, child reads, listings, directory facts and rechecks.
//! - `state.rs`: `read_bounded_inner` (every outcome), the intent journal
//!   directory and its entries, `StateLock::shared_if_present` (held, absent
//!   and refused) and `StateLock::exclusive` (recorded as a lock-create
//!   effect), the `LockSet` shared acquisitions, and every `now_ms` reading.
//! - `registry.rs`, `reference.rs`, `pool_custody.rs`, `identity.rs`,
//!   `area.rs`, `custody.rs`: the named source sites' own rows, so a direct
//!   read outside `DirOps` is taped at its source symbol.
//!
//! `area::show` adds no distinct read of its own: it is served by the
//! `area::read_declarations` rows recorded below it, which bind each
//! declaration file's path, bytes digest and cap. The write-side `DirOps`
//! verbs that the plan never reaches (`create_child_dir`,
//! `publish_child_file`, `remove_child`, `rename_child`) record effect rows,
//! as attempts. Their individual syscall outcomes and parent identities remain
//! incomplete, so they cannot establish an exact effect set. Not recorded, because the plan never calls
//! them and they read no fixture object: `random_bytes` (entropy, not fixture
//! state), `set_dir_mode`, `create_child_symlink`, `sync_dir` and the flush
//! path.
//!
//! Nothing here is public and nothing here feeds a production decision; the
//! no-public-surface test below pins the declaration shape. The source-count
//! check does not prove per-site cfg reachability or complete coverage.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::digest::sha256_hex;

#[path = "capture_wire.rs"]
mod capture_wire;

/// The reviewed shared transcript schema the adapter emits.
pub(crate) const TRANSCRIPT_SCHEMA: &str = capture_wire::TRANSCRIPT_SCHEMA;
/// The reviewed shared manifest schema the harness reads and binds.
pub(crate) const MANIFEST_SCHEMA: &str = capture_wire::MANIFEST_SCHEMA;
/// The recorder's own diagnostic document. It carries caps, provenance flags,
/// the fixture capture and the completeness report; it is not a capture schema
/// and is never proof input.
pub(crate) const RECORDER_SCHEMA: &str = "barge.capture.recorder/1";
/// A distinct apparatus diagnostic for a refusal before the engine ran or a
/// serialization fallback. The transcript parser refuses it, so it can never be
/// read as a conforming capture.
pub(crate) const APPARATUS_SCHEMA: &str = "oxide.capture.apparatus/2";
/// The engine name this crate's window writes.
pub(crate) const ENGINE: &str = "barge";
/// The fixture-entry cap on each side of the fixture capture.
pub(crate) const CAP_FIXTURE_ENTRIES: usize = 4096;
/// The bound on bytes hashed across one fixture capture.
const CAP_FIXTURE_BYTES: u64 = 32 * 1024 * 1024;
/// The transcript's read-row cap.
pub(crate) const CAP_READ_ROWS: usize = 8192;
/// The transcript's document size cap.
pub(crate) const CAP_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
/// The transcript's per-identity-key occurrence cap.
pub(crate) const CAP_OCCURRENCES: u32 = 64;
/// The transcript's process-row cap.
pub(crate) const CAP_PROCESS_ROWS: usize = 512;

/// Facts actually supplied for an observation. Missing fields remain absent;
/// a lexical path alone is not a complete object identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provenance {
    Handle,
    Extracted,
    Context,
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_inode: Option<u64>,
    #[serde(default)]
    parent_source: Provenance,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inode: Option<u64>,
    #[serde(default)]
    own_source: Provenance,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fd: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mount: Option<String>,
    // The device a volume was observed as (a device path). This recorder does
    // not observe it today; the shared wire requires the key, so it is carried
    // explicitly as absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_device: Option<String>,
}

impl Identity {
    /// An identity for a fact with no object of its own (the effective uid, the
    /// boot identity, a whole-table answer).
    pub(crate) fn none() -> Self {
        Self {
            path: None,
            component: None,
            parent_device: None,
            parent_inode: None,
            parent_source: Provenance::None,
            device: None,
            inode: None,
            own_source: Provenance::None,
            pid: None,
            start: None,
            uid: None,
            fd: None,
            volume_uuid: None,
            mount: None,
            volume_device: None,
        }
    }

    /// A path and its own device and inode when the caller read them.
    /// No parent is inferred from a separate lookup.
    pub(crate) fn file(path: &Path, facts: Option<(u64, u64)>) -> Self {
        Self {
            path: path.to_str().map(str::to_owned),
            component: None,
            device: facts.map(|(device, _)| device),
            inode: facts.map(|(_, inode)| inode),
            own_source: if facts.is_some() {
                Provenance::Extracted
            } else {
                Provenance::None
            },
            ..Self::none()
        }
    }

    /// A path component bound to the parent it was observed under, when the
    /// parent could be stated.
    pub(crate) fn component(parent: Option<(u64, u64)>, name: &str) -> Self {
        Self {
            component: Some(name.to_owned()),
            parent_device: parent.map(|(device, _)| device),
            parent_inode: parent.map(|(_, inode)| inode),
            parent_source: if parent.is_some() {
                Provenance::Context
            } else {
                Provenance::None
            },
            ..Self::none()
        }
    }

    /// A pid whose process identity was not observed; never a positive identity.
    pub(crate) fn unidentified_process(pid: u32, fd: Option<i32>) -> Self {
        Self {
            pid: Some(pid),
            fd,
            ..Self::none()
        }
    }

    /// A process identity: never a pid alone.
    pub(crate) fn process(pid: u32, start: u64, uid: Option<u32>, fd: Option<i32>) -> Self {
        Self {
            pid: Some(pid),
            start: (start != 0).then_some(start),
            uid,
            fd,
            ..Self::none()
        }
    }

    /// A volume identity: the UUID and the mount it was observed at.
    pub(crate) fn volume(volume_uuid: Option<&str>, mount: Option<&Path>) -> Self {
        Self {
            volume_uuid: volume_uuid.map(str::to_owned),
            mount: mount.and_then(Path::to_str).map(str::to_owned),
            ..Self::none()
        }
    }

    /// A volume identity observed at a path: the path and its device, the
    /// UUID, and the kernel's mount point. The device number was observed at
    /// this site, so the own identity is `extracted`.
    pub(crate) fn volume_at(
        path: &Path,
        device: u64,
        volume_uuid: Option<&str>,
        mount: Option<&Path>,
    ) -> Self {
        Self {
            path: path.to_str().map(str::to_owned),
            device: Some(device),
            volume_uuid: volume_uuid.map(str::to_owned),
            mount: mount.and_then(Path::to_str).map(str::to_owned),
            own_source: Provenance::Extracted,
            ..Self::none()
        }
    }
}

/// What one observation returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The bytes that were read, bound by digest, with the cap that bound them.
    Digest {
        bytes_sha256: String,
        size: u64,
        cap: Option<u64>,
    },
    /// The entry is there, with what it is.
    Facts { kind: String },
    /// The object is positively not there.
    Absent,
    /// An enumeration: how many the directory held, how many were kept, and
    /// whether names were left out.
    Listed {
        total: u64,
        kept: u64,
        truncated: bool,
    },
    /// A scalar or classified answer: an errno class, a verdict class, a read
    /// value such as a uid or a boot identity.
    Answer { answer: String },
    /// The observation could not be completed, with its refusal class.
    Refused { class: String },
}

impl Outcome {
    /// A bytes-digest outcome.
    pub(crate) fn digest(bytes: &[u8], cap: Option<u64>) -> Self {
        Self::Digest {
            bytes_sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            cap,
        }
    }

    /// An explicit absence.
    pub(crate) fn absent() -> Self {
        Self::Absent
    }

    /// A presence outcome with the entry's kind.
    pub(crate) fn facts(kind: &str) -> Self {
        Self::Facts {
            kind: kind.to_owned(),
        }
    }

    /// An enumeration outcome.
    pub(crate) fn listed(total: usize, kept: usize, truncated: bool) -> Self {
        Self::Listed {
            total: total as u64,
            kept: kept as u64,
            truncated,
        }
    }

    /// A classified or scalar answer.
    pub(crate) fn answer(answer: impl Into<String>) -> Self {
        Self::Answer {
            answer: answer.into(),
        }
    }

    /// A refusal class.
    pub(crate) fn refused(class: impl Into<String>) -> Self {
        Self::Refused {
            class: class.into(),
        }
    }
}

/// One leaf observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReadRow {
    /// The observation family.
    kind: &'static str,
    /// The source symbol whose statement recorded it.
    site: &'static str,
    identity: Identity,
    outcome: Outcome,
    // Whether this row has the identity required for its observation family.
    observation_proven: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cap: Option<u64>,
    epoch: u64,
    occurrence: u32,
}

/// One observed or attempted effect. The plan's own window records none; a
/// non-empty set is the honest record of an attempted write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EffectRow {
    kind: &'static str,
    site: String,
    path: Option<String>,
    // Retain the exact path only for matching the result to its attempt. The
    // shared wire still refuses to invent UTF-8 text: an unrepresentable path
    // becomes a descriptor effect with no path and its observed result.
    #[serde(skip)]
    match_path: PathBuf,
    parent_device: Option<u64>,
    parent_inode: Option<u64>,
    device: Option<u64>,
    inode: Option<u64>,
    destination: Option<String>,
    outcome: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    partial_effect_possible: bool,
    epoch: u64,
}

/// One clock reading bound to the declared window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ClockRow {
    kind: &'static str,
    site: String,
    value_ms: u64,
    epoch: u64,
}

/// Which cap hit its bound, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Caps {
    read_rows: usize,
    document_bytes: usize,
    occurrences_per_key: u32,
    process_rows: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    read_rows_exhausted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    occurrences_exhausted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    process_rows_exhausted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    document_truncated: bool,
    dropped_rows: u64,
}

/// The window a caller opened. The harness opens exactly one per engine run.
#[derive(Debug, Clone)]
pub(crate) struct WindowConfig {
    /// The case name from the manifest.
    pub(crate) case: String,
    /// The manifest bytes' digest, bound into every row's document.
    pub(crate) manifest_sha256: String,
    /// The entrypoint symbol the window calls.
    pub(crate) entrypoint_symbol: &'static str,
    /// The entrypoint's source binding, as the orchestrator staged it.
    pub(crate) source_binding: String,
}

/// The engine's own outcome, in its own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EngineOutcome {
    /// `completed` or `refused`.
    pub(crate) status: &'static str,
    /// The engine's own exit code for that outcome.
    pub(crate) exit_code: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) refusal_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) refusal_message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) refusal_detail: Vec<(String, serde_json::Value)>,
    /// Whether the engine's own caps cut its result short.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) engine_truncated: bool,
}

/// One recorded pool or lock holder, with the kernel facts it actually
/// supplied. A holder whose kernel start or uid was not observed keeps an
/// explicit `null`; completeness then reports the lifetime as unproven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RecordedHolder {
    site: String,
    record_id: u64,
    pid: u32,
    recorded_start_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    epoch: u64,
}

/// One fixture-tree entry, as the apparatus observed it before or after the
/// window. The wire names are the shared schema's `device` and `inode`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FixtureEntry {
    rel: String,
    kind: &'static str,
    size: u64,
    device: u64,
    inode: u64,
    parent_device: Option<u64>,
    parent_inode: Option<u64>,
    mode: u32,
    bytes_sha256: Option<String>,
}

/// One window's bounded fixture capture: the tree before the engine ran, after
/// it finished, and the net change between them. A fixture diff is an observed
/// net change whose writer result was not observed, so every diff effect is
/// `unknown` at epoch zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FixtureCapture {
    root: String,
    truncated: bool,
    pre: Vec<FixtureEntry>,
    post: Vec<FixtureEntry>,
}

/// The recorder's own document. Field order is the serialization order. This
/// document is a diagnostic, not a capture schema: it carries the caps, the
/// per-row provenance flag and the fixture capture that the shared wire has no
/// field for, and it is never proof input.
#[derive(Debug, Serialize)]
struct RecorderDocument<'a> {
    schema: &'static str,
    engine: &'static str,
    case: &'a str,
    manifest_sha256: &'a str,
    entrypoint: Entrypoint<'a>,
    window_elapsed_ns: u64,
    outcome: &'a EngineOutcome,
    /// This recorder has not established complete source coverage.
    observation_complete: bool,
    /// The engine's own result in its own document, when it completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a serde_json::Value>,
    effects: &'a [EffectRow],
    reads: &'a [ReadRow],
    clocks: &'a [ClockRow],
    recorded_holders: &'a [RecordedHolder],
    fixture_capture: Option<serde_json::Value>,
    notes: Vec<String>,
    caps: Caps,
}

/// The entrypoint identity the recorder names.
#[derive(Debug, Serialize)]
struct Entrypoint<'a> {
    symbol: &'static str,
    source_binding: &'a str,
}

/// The per-thread recorder state. The plan is single-threaded, so one
/// thread-local window holds every leaf the plan reaches; a lib test that
/// never opens a window records nothing.
#[derive(Debug)]
struct Tape {
    config: Option<WindowConfig>,
    started: Option<Instant>,
    fixture_root: Option<PathBuf>,
    fixture_pre: Vec<FixtureEntry>,
    fixture_truncated: bool,
    reads: Vec<ReadRow>,
    clocks: Vec<ClockRow>,
    effects: Vec<EffectRow>,
    holders: Vec<RecordedHolder>,
    occurrences: HashMap<(&'static str, &'static str, Identity), u32>,
    process_rows: usize,
    holder_rows: usize,
    next_epoch: u64,
    read_rows_exhausted: bool,
    occurrences_exhausted: bool,
    process_rows_exhausted: bool,
    holders_exhausted: bool,
    dropped_rows: u64,
    manifest: Option<serde_json::Value>,
    unrepresentable_effect_noted: bool,
    notes: Vec<String>,
}

impl Tape {
    /// Drop every row and counter but leave the (already taken) window gone, so
    /// the next window starts from epoch one and no rows.
    fn reset_without_window(&mut self) {
        *self = Self::empty();
    }

    /// A tape with no window and no rows, as every thread starts.
    fn empty() -> Self {
        Self {
            config: None,
            started: None,
            fixture_root: None,
            fixture_pre: Vec::new(),
            fixture_truncated: false,
            reads: Vec::new(),
            clocks: Vec::new(),
            effects: Vec::new(),
            holders: Vec::new(),
            occurrences: HashMap::new(),
            process_rows: 0,
            holder_rows: 0,
            next_epoch: 1,
            read_rows_exhausted: false,
            occurrences_exhausted: false,
            process_rows_exhausted: false,
            holders_exhausted: false,
            dropped_rows: 0,
            manifest: None,
            unrepresentable_effect_noted: false,
            notes: Vec::new(),
        }
    }

    fn push_note(&mut self, note: String) {
        if self.notes.len() < CAP_READ_ROWS {
            self.notes.push(note);
        }
    }
}

/// Attach the manifest the window is run under. Completeness is then judged by
/// the shared parser and reconciler rules over the emitted transcript, never by
/// a second projection here.
pub(crate) fn attach_manifest(manifest: &[u8]) {
    let document = serde_json::from_slice::<serde_json::Value>(manifest).ok();
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_some() {
            tape.manifest = document;
        }
    });
}

/// Capture the fixture tree before the engine runs. The capture is apparatus,
/// not an observation fed to the engine: it reads the fixture root the caller
/// already owns, bounded to 4096 entries and 32 MiB of hashed bytes.
pub(crate) fn begin_fixture(root: &Path) {
    let (entries, truncated) = capture_fixture(root);
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_some() {
            tape.fixture_truncated = truncated;
            tape.fixture_pre = entries;
            tape.fixture_root = Some(root.to_path_buf());
        }
    });
}

thread_local! {
    static TAPE: RefCell<Tape> = RefCell::new(Tape::empty());
    static PROCESS_CONTEXT: RefCell<Option<(u32, u64, Option<u32>)>> = const { RefCell::new(None) };
    static FAIL_DUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn fail_next_dup() {
    FAIL_DUP.with(|fail| fail.set(true));
}

pub(crate) fn take_dup_failure() -> bool {
    FAIL_DUP.with(|fail| fail.replace(false))
}

/// Bind subsequent descriptor observations to the evaluator's existing identity.
pub(crate) fn process_context(pid: u32, start: u64, uid: Option<u32>) {
    PROCESS_CONTEXT.with(|slot| *slot.borrow_mut() = Some((pid, start, uid)));
}

fn process_identity(pid: u32, fd: Option<i32>) -> Identity {
    PROCESS_CONTEXT.with(|slot| match *slot.borrow() {
        Some((bound_pid, start, uid)) if bound_pid == pid => Identity::process(pid, start, uid, fd),
        _ => Identity::unidentified_process(pid, fd),
    })
}

/// Open this thread's window. A window that is already open is refused: one
/// window per run, never nested.
pub(crate) fn begin_window(config: WindowConfig) -> Result<(), String> {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_some() {
            return Err("a tape window is already open on this thread".to_owned());
        }
        PROCESS_CONTEXT.with(|slot| *slot.borrow_mut() = None);
        let mut notes = vec![format!(
            "source binding: {}",
            config.source_binding
        )];
        notes.push(
            "observation coverage, K1 parent lifetime and process facts at the operation remain unproven"
                .to_owned(),
        );
        tape.config = Some(config);
        tape.started = Some(Instant::now());
        tape.notes = notes;
        Ok(())
    })
}

/// Close the window and serialize the recorder document. The tape resets, so a
/// later window starts from epoch one and no rows.
pub(crate) fn finish(
    outcome: EngineOutcome,
    result: Option<&serde_json::Value>,
) -> Result<String, String> {
    finish_inner(outcome, result)
}

/// Close the window as an apparatus diagnostic: the engine's own report or
/// refusal could not be carried into a conforming transcript, so the capture is
/// not a capture. The reason states the real engine status and exit code.
pub(crate) fn finish_apparatus(reason: &str) -> Result<String, String> {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        let config = tape
            .config
            .take()
            .ok_or_else(|| "no tape window is open on this thread".to_owned())?;
        tape.reset_without_window();
        Ok(apparatus_document(
            &config.case,
            Some(config.manifest_sha256.as_str()),
            reason,
        ))
    })
}

/// A distinct apparatus diagnostic. It is never a conforming capture: the
/// transcript parser refuses its schema by name. The reason is bounded, and the
/// full reason's digest is always recorded beside it, so untrusted diagnostic
/// text can never overflow the document.
pub(crate) fn apparatus_document(
    case: &str,
    manifest_sha256: Option<&str>,
    reason: &str,
) -> String {
    const MAX_REASON: usize = 16 * 1024;
    let bounded = if reason.len() > MAX_REASON {
        let mut cut = MAX_REASON;
        while cut > 0 && !reason.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{} [truncated]", &reason[..cut])
    } else {
        reason.to_owned()
    };
    let document = serde_json::json!({
        "schema": APPARATUS_SCHEMA,
        "version": APPARATUS_SCHEMA,
        "case": case,
        "manifest_sha256": manifest_sha256,
        "reason": bounded,
        "reason_sha256": sha256_hex(reason.as_bytes()),
    });
    serde_json::to_string(&document).unwrap_or_else(|_| {
        format!(
            r#"{{"schema":"{APPARATUS_SCHEMA}","version":"{APPARATUS_SCHEMA}","case":"","manifest_sha256":null,"reason":"apparatus diagnostic could not be serialized","reason_sha256":null}}"#
        )
    })
}

/// Translate this run's recorder document into the reviewed `oxide.capture.transcript/2`
/// shape. Every available row fact is preserved, absence is an explicit null,
/// and the shared parser must accept the result. Historical `/1` packets are
/// never passed through this adapter and are never upgraded.
pub(crate) fn shared_transcript(transcript: &str) -> Result<String, String> {
    let recorder: serde_json::Value =
        serde_json::from_str(transcript).map_err(|error| error.to_string())?;
    if recorder["schema"] == APPARATUS_SCHEMA {
        return Err("an apparatus diagnostic is not a transcript".to_owned());
    }
    let case = recorder["case"]
        .as_str()
        .ok_or("the recorder document has no case")?;
    let outcome = wire_outcome(&recorder)?;
    let manifest_sha256 = recorder["manifest_sha256"]
        .as_str()
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
    let entrypoint = recorder["entrypoint"]["symbol"]
        .as_str()
        .ok_or("the recorder document has no entrypoint symbol")?;
    let truncation = &recorder["caps"];
    let (reads, occurrences_truncated) = wire_reads(&recorder)?;
    let wire = serde_json::json!({
        "schema": TRANSCRIPT_SCHEMA,
        "version": TRANSCRIPT_SCHEMA,
        "engine": ENGINE,
        "case": case,
        "manifest_sha256": manifest_sha256,
        "entrypoint": entrypoint,
        // No absolute window origin is proven, so both ends stay an explicit null.
        "window_monotonic_start_ns": serde_json::Value::Null,
        "window_monotonic_end_ns": serde_json::Value::Null,
        "window_elapsed_ns": recorder["window_elapsed_ns"].as_u64().unwrap_or(0),
        "outcome": outcome,
        "truncation": {
            "read_rows_truncated": truncation["read_rows_exhausted"].as_bool().unwrap_or(false),
            "occurrences_truncated": truncation["occurrences_exhausted"].as_bool().unwrap_or(false)
                || occurrences_truncated,
            "document_truncated": truncation["document_truncated"].as_bool().unwrap_or(false),
            "process_rows_truncated": truncation["process_rows_exhausted"].as_bool().unwrap_or(false),
            // This evaluator records no separate classification family.
            "classifications_truncated": false,
        },
        "effects": wire_effects(&recorder)?,
        "reads": reads,
        "clocks": wire_clocks(&recorder)?,
        "classifications": [],
        "recorded_holders": wire_holders(&recorder),
        "fixture_capture": wire_fixture_capture(&recorder),
        "notes": recorder["notes"].clone(),
    });
    let bytes = serde_json::to_string(&wire).map_err(|error| error.to_string())?;
    capture_wire::parse_transcript(bytes.as_bytes()).map_err(|error| error.message)?;
    Ok(bytes)
}

/// Judge the emitted transcript with the shared parser and reconciler rules.
/// This is the only completeness implementation in this crate: it does not
/// keep a second projection.
pub(crate) fn assess_shared(
    transcript: &str,
    manifest: &[u8],
) -> Result<capture_wire::Completeness, String> {
    let recorded =
        capture_wire::parse_transcript(transcript.as_bytes()).map_err(|error| error.message)?;
    let manifest = capture_wire::parse_manifest(manifest).map_err(|error| error.message)?;
    Ok(capture_wire::assess_completeness(
        &recorded,
        Some(&manifest),
    ))
}

pub(crate) fn validate_shared_transcript(bytes: &[u8]) -> Result<(), String> {
    capture_wire::parse_transcript(bytes)
        .map(|_| ())
        .map_err(|error| error.message)
}

pub(crate) fn validate_shared_manifest(bytes: &[u8]) -> Result<(), String> {
    capture_wire::parse_manifest(bytes)
        .map(|_| ())
        .map_err(|error| error.message)
}

/// The engine outcome in the shared vocabulary. A refusal names the engine as
/// its source; a completed window carries the engine's own result object.
fn wire_outcome(recorder: &serde_json::Value) -> Result<serde_json::Value, String> {
    let engine = &recorder["outcome"];
    match engine["status"].as_str() {
        Some("completed") => {
            let result = &recorder["result"];
            if !result.is_object() {
                return Err("a completed window has no engine result object to carry".to_owned());
            }
            Ok(serde_json::json!({ "state": "completed", "result": result }))
        }
        Some("refused") => {
            let reason = match (
                engine["refusal_kind"].as_str(),
                engine["refusal_message"].as_str(),
            ) {
                (Some(kind), Some(message)) => format!("{kind}: {message}"),
                (Some(kind), None) => kind.to_owned(),
                (None, Some(message)) => message.to_owned(),
                (None, None) => "refused".to_owned(),
            };
            Ok(serde_json::json!({
                "state": "refused",
                "reason": reason,
                "exit_code": engine["exit_code"].as_u64().unwrap_or(2),
                "source": "engine",
            }))
        }
        other => Err(format!("unknown engine outcome status {other:?}")),
    }
}

/// The wire read rows, and whether the wire's own occurrence cap cut a row.
///
/// The reviewed wire requires `kind` to equal `site`, so Barge's `site` is the
/// source symbol with the observation family appended (`symbol#family`). One
/// source symbol can record several leaf families, and each family is its own
/// reconcilable key. The recorder counts occurrences per complete internal
/// identity; the wire's identity key is narrower (it has no fd, pid, start, uid
/// or volume UUID), so the adapter renumbers occurrences per emitted key in
/// emitted order. Every row keeps its own outcome and epoch; a key that would
/// exceed the 64-occurrence cap stops emitting rows for that key, records the
/// wire truncation and leaves the rows in the recorder document.
fn wire_reads(recorder: &serde_json::Value) -> Result<(Vec<serde_json::Value>, bool), String> {
    let rows = recorder["reads"]
        .as_array()
        .ok_or("the recorder document has no read rows")?;
    let mut reads = Vec::with_capacity(rows.len());
    let mut counts: HashMap<(String, String, String, String, String), u64> = HashMap::new();
    let mut truncated = false;
    for row in rows {
        let symbol = row["site"].as_str().ok_or("a read row has no site")?;
        let family = row["kind"].as_str().ok_or("a read row has no kind")?;
        let site = format!("{symbol}#{family}");
        let identity = row["identity"]
            .as_object()
            .ok_or("a read row has no identity")?;
        let wire_identity = wire_identity(identity);
        let part = |value: &serde_json::Value| match value {
            serde_json::Value::Null => "none".to_owned(),
            other => other.to_string(),
        };
        let key = (
            site.clone(),
            part(&wire_identity["path"]),
            part(&wire_identity["component"]),
            part(&wire_identity["parent_device"]),
            part(&wire_identity["parent_inode"]),
        );
        let occurrence = counts.entry(key).or_insert(0);
        *occurrence = occurrence.saturating_add(1);
        if *occurrence > u64::from(CAP_OCCURRENCES) {
            truncated = true;
            continue;
        }
        reads.push(serde_json::json!({
            "kind": site,
            "site": site,
            "identity": wire_identity,
            "outcome": wire_outcome_of_row(row),
            "cap": row["cap"].as_u64(),
            "epoch": row["epoch"].as_u64().ok_or("a read row has no epoch")?,
            "occurrence": occurrence,
        }));
    }
    Ok((reads, truncated))
}

/// Every identity key is present; a missing fact is an explicit null. The
/// parent pair and its provenance, and the own pair and its provenance, are
/// emitted together so the shared pair rules hold by construction.
fn wire_identity(identity: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let value = |key: &str| {
        identity
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    let pair = |device: &str, inode: &str| {
        identity
            .get(device)
            .and_then(serde_json::Value::as_u64)
            .zip(identity.get(inode).and_then(serde_json::Value::as_u64))
    };
    let parent = pair("parent_device", "parent_inode");
    let own = pair("device", "inode");
    let parent_source = if parent.is_some() {
        identity
            .get("parent_source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("context")
            .to_owned()
    } else {
        "none".to_owned()
    };
    let own_source = if own.is_some() {
        identity
            .get("own_source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("extracted")
            .to_owned()
    } else {
        "none".to_owned()
    };
    serde_json::json!({
        "path": value("path"),
        "component": value("component"),
        "parent_device": parent.map(|(device, _)| device),
        "parent_inode": parent.map(|(_, inode)| inode),
        "device": own.map(|(device, _)| device),
        "inode": own.map(|(_, inode)| inode),
        "pid": value("pid"),
        "start": value("start"),
        "uid": value("uid"),
        "fd": value("fd"),
        "volume_uuid": value("volume_uuid"),
        "mount": value("mount"),
        "volume_device": value("volume_device"),
        "parent_source": parent_source,
        "own_source": own_source,
    })
}

/// One observation's outcome in the shared vocabulary. A listed count, an
/// answer and a presence fact are carried as the `facts` string; the digest,
/// absence and refusal shapes are explicit so nothing is inferred from text.
fn wire_outcome_of_row(row: &serde_json::Value) -> serde_json::Value {
    let outcome = &row["outcome"];
    if let Some(digest) = outcome.pointer("/digest/bytes_sha256") {
        return serde_json::json!({ "type": "digest", "digest": digest });
    }
    if outcome == "absent" {
        return serde_json::json!({ "type": "absent" });
    }
    if let Some(reason) = outcome.pointer("/refused/class") {
        return serde_json::json!({ "type": "refusal", "reason": reason });
    }
    if let Some(facts) = outcome.pointer("/facts/kind") {
        return serde_json::json!({ "type": "facts", "facts": facts });
    }
    if let Some(answer) = outcome.pointer("/answer/answer") {
        return serde_json::json!({ "type": "facts", "facts": answer });
    }
    // An enumeration keeps its counts and truncation flag as the facts text of
    // its own outcome object; no field is dropped.
    serde_json::json!({ "type": "facts", "facts": outcome.to_string() })
}

fn wire_effects(recorder: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    let rows = recorder["effects"]
        .as_array()
        .ok_or("the recorder document has no effect rows")?;
    let mut effects = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = row["kind"].as_str().ok_or("an effect row has no kind")?;
        let site = row["site"].as_str().ok_or("an effect row has no site")?;
        let path = row["path"].as_str();
        let partial = row["partial_effect_possible"].as_bool().unwrap_or(false);
        let (attempted, result) = match row["outcome"].as_str() {
            None => (true, "unknown"),
            Some("succeeded") => (false, "succeeded"),
            Some(_) if partial => (false, "partial"),
            Some(_) => (false, "failed"),
        };
        // An unrepresentable path has no wire path field. The effect keeps its
        // observed result as a descriptor effect, whose identity stays unproven.
        let (kind, path) = if path.is_none() {
            ("descriptor-effect", None)
        } else {
            if !matches!(
                kind,
                "create" | "save" | "fence" | "reconcile" | "lock-create" | "rename" | "unlink"
            ) {
                return Err(format!("effect kind {kind} has no shared wire mapping"));
            }
            (kind, path)
        };
        effects.push(serde_json::json!({
            "kind": kind,
            "site": site,
            "attempted": attempted,
            "path": path,
            "epoch": row["epoch"].as_u64().ok_or("an effect row has no epoch")?,
            "result": result,
        }));
    }
    Ok(effects)
}

fn wire_clocks(recorder: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    let rows = recorder["clocks"]
        .as_array()
        .ok_or("the recorder document has no clock rows")?;
    rows.iter()
        .map(|row| {
            Ok(serde_json::json!({
                "kind": row["kind"].as_str().ok_or("a clock row has no kind")?,
                "value": row["value_ms"].as_u64(),
                "epoch": row["epoch"].as_u64().ok_or("a clock row has no epoch")?,
            }))
        })
        .collect()
}

fn wire_holders(recorder: &serde_json::Value) -> Vec<serde_json::Value> {
    recorder["recorded_holders"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    serde_json::json!({
                        "site": row["site"],
                        "record_id": row["record_id"],
                        "pid": row["pid"],
                        "recorded_start_ms": row["recorded_start_ms"],
                        "kernel_start": row["kernel_start"],
                        "uid": row["uid"],
                        "epoch": row["epoch"],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn wire_fixture_capture(recorder: &serde_json::Value) -> serde_json::Value {
    let capture = &recorder["fixture_capture"];
    if !capture.is_object() {
        return serde_json::Value::Null;
    }
    serde_json::json!({
        "root": capture["root"],
        "truncated": capture["truncated"],
        "pre": wire_fixture_entries(&capture["pre"]),
        "post": wire_fixture_entries(&capture["post"]),
        "diff_effects": capture["diff_effects"],
    })
}

fn wire_fixture_entries(entries: &serde_json::Value) -> Vec<serde_json::Value> {
    entries
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|entry| {
                    serde_json::json!({
                        "rel": entry["rel"],
                        "kind": entry["kind"],
                        "size": entry["size"],
                        "device": entry["device"],
                        "inode": entry["inode"],
                        "parent_device": entry["parent_device"],
                        "parent_inode": entry["parent_inode"],
                        "mode": entry["mode"],
                        "bytes_sha256": entry["bytes_sha256"],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---- fixture-capture apparatus ----
//
// Everything below reads the fixture tree the caller already owns, before and
// after the engine window. It is apparatus for binding fixture identity and net
// change; it states no engine observation and feeds the engine nothing.

/// One bounded walk of the fixture tree, as the apparatus observed it. The
/// capture reads only the fixture root the caller owns, before the engine runs
/// and again after it finishes; it feeds nothing to the engine and states no
/// engine observation.
fn capture_fixture(root: &Path) -> (Vec<FixtureEntry>, bool) {
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut remaining = CAP_FIXTURE_BYTES;
    walk_fixture(root, root, &mut entries, &mut truncated, &mut remaining);
    (entries, truncated)
}

fn walk_fixture(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<FixtureEntry>,
    truncated: &mut bool,
    remaining: &mut u64,
) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        *truncated = true;
        return;
    };
    let parent = std::fs::metadata(dir).ok();
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in read_dir {
        match entry {
            Ok(entry) => {
                // A lossy name could alias a different fixture entry.
                if entry.file_name().to_str().is_some() {
                    paths.push(entry.path());
                } else {
                    *truncated = true;
                }
            }
            Err(_) => *truncated = true,
        }
    }
    paths.sort();
    for path in paths {
        if entries.len() >= CAP_FIXTURE_ENTRIES {
            *truncated = true;
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            *truncated = true;
            continue;
        };
        let Some(rel) = path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| rel.to_str())
            .map(str::to_owned)
        else {
            *truncated = true;
            continue;
        };
        let kind = if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_dir() {
            "dir"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        let bytes_sha256 = if meta.is_file() {
            match read_fixture_file(&path, remaining) {
                Some(digest) => Some(digest),
                None => {
                    *truncated = true;
                    None
                }
            }
        } else {
            None
        };
        entries.push(FixtureEntry {
            rel: rel.clone(),
            kind,
            size: meta.len(),
            device: meta.dev(),
            inode: meta.ino(),
            parent_device: parent.as_ref().map(std::os::unix::fs::MetadataExt::dev),
            parent_inode: parent.as_ref().map(std::os::unix::fs::MetadataExt::ino),
            mode: meta.mode() & 0o7777,
            bytes_sha256,
        });
        if meta.is_dir() {
            walk_fixture(root, &path, entries, truncated, remaining);
        }
    }
}

/// Hash one fixture file inside the remaining byte budget. A file that cannot
/// be read, or that would exceed the budget, carries no digest and truncates
/// the capture rather than silently ignoring the bytes.
fn read_fixture_file(path: &Path, remaining: &mut u64) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| {
        file.take(remaining.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    match read {
        Ok(_) if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= *remaining => {
            *remaining = remaining.saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            Some(sha256_hex(&bytes))
        }
        _ => None,
    }
}

/// The fixture capture the recorder document carries: root, both sides and the
/// net change. A diff effect is an observed net change with no observed writer
/// result, so it is `unknown` at epoch zero and never completed parity.
fn fixture_capture_document(
    root: &Path,
    pre: &[FixtureEntry],
    truncated: bool,
) -> serde_json::Value {
    let (post, post_truncated) = capture_fixture(root);
    let mut diff = Vec::new();
    for entry in &post {
        match pre.iter().find(|before| before.rel == entry.rel) {
            None => diff.push(fixture_diff_effect("create", &entry.rel)),
            Some(before) => {
                let is_dir = entry.kind == "dir";
                let changed = before.device != entry.device
                    || before.inode != entry.inode
                    || before.mode != entry.mode
                    || (!is_dir
                        && (before.size != entry.size
                            || before.bytes_sha256 != entry.bytes_sha256))
                    || (is_dir && before.bytes_sha256 != entry.bytes_sha256);
                if changed {
                    diff.push(fixture_diff_effect("save", &entry.rel));
                }
            }
        }
    }
    for entry in pre {
        if !post.iter().any(|after| after.rel == entry.rel) {
            diff.push(fixture_diff_effect("unlink", &entry.rel));
        }
    }
    diff.sort_by(|left, right| {
        left["path"]
            .as_str()
            .cmp(&right["path"].as_str())
            .then(left["kind"].as_str().cmp(&right["kind"].as_str()))
    });
    serde_json::json!({
        "root": root.display().to_string(),
        "truncated": truncated || post_truncated,
        "pre": pre.iter().map(fixture_entry_document).collect::<Vec<_>>(),
        "post": post.iter().map(fixture_entry_document).collect::<Vec<_>>(),
        "diff_effects": diff,
    })
}

fn fixture_entry_document(entry: &FixtureEntry) -> serde_json::Value {
    serde_json::json!({
        "rel": entry.rel,
        "kind": entry.kind,
        "size": entry.size,
        "device": entry.device,
        "inode": entry.inode,
        "parent_device": entry.parent_device,
        "parent_inode": entry.parent_inode,
        "mode": entry.mode,
        "bytes_sha256": entry.bytes_sha256,
    })
}

fn fixture_diff_effect(kind: &str, rel: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": kind,
        "site": "fixture-diff",
        "attempted": false,
        "path": rel,
        "epoch": 0,
        "result": "unknown",
    })
}

fn finish_inner(
    outcome: EngineOutcome,
    result: Option<&serde_json::Value>,
) -> Result<String, String> {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        let config = tape
            .config
            .take()
            .ok_or_else(|| "no tape window is open on this thread".to_owned())?;
        if result.is_some() && outcome.status != "completed" {
            return Err("a refused window carries no engine result".to_owned());
        }
        if result.is_none() && outcome.status != "refused" {
            return Err("a completed window carries the engine result".to_owned());
        }
        if tape.effects.iter().any(|row| row.path.is_none()) {
            tape.push_note(
                "an effect with an unrepresentable path is emitted as a descriptor effect \
                 with no path; its identity stays unproven"
                    .to_owned(),
            );
        }
        if tape
            .effects
            .iter()
            .any(|row| row.parent_device.is_some() || row.destination.is_some())
        {
            tape.push_note(
                "child-effect parent identity and rename destination are recorded in this \
                 diagnostic document only; the shared wire has no field for them"
                    .to_owned(),
            );
        }
        let elapsed_ns = tape
            .started
            .map(|started| u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let fixture_capture = tape
            .fixture_root
            .as_ref()
            .map(|root| fixture_capture_document(root, &tape.fixture_pre, tape.fixture_truncated));
        let mut caps = Caps {
            read_rows: tape.reads.len(),
            document_bytes: 0,
            occurrences_per_key: CAP_OCCURRENCES,
            process_rows: tape.process_rows,
            read_rows_exhausted: tape.read_rows_exhausted,
            occurrences_exhausted: tape.occurrences_exhausted,
            process_rows_exhausted: tape.process_rows_exhausted || tape.holders_exhausted,
            document_truncated: false,
            dropped_rows: tape.dropped_rows,
        };
        let mut reads = std::mem::take(&mut tape.reads);
        // The document cap is a contract cap, not a suggestion: rows leave from
        // the tail until the document fits, and the truncation is recorded.
        loop {
            let document = RecorderDocument {
                schema: RECORDER_SCHEMA,
                engine: ENGINE,
                case: &config.case,
                manifest_sha256: &config.manifest_sha256,
                entrypoint: Entrypoint {
                    symbol: config.entrypoint_symbol,
                    source_binding: &config.source_binding,
                },
                window_elapsed_ns: elapsed_ns,
                outcome: &outcome,
                observation_complete: false,
                result,
                effects: &tape.effects,
                reads: &reads,
                clocks: &tape.clocks,
                recorded_holders: &tape.holders,
                fixture_capture: fixture_capture.clone(),
                notes: tape.notes.clone(),
                caps,
            };
            let bytes = serde_json::to_string(&document)
                .map_err(|error| format!("the recorder document cannot be serialized: {error}"))?;
            // The decimal length field itself contributes to the encoded size.
            // Iterate to a fixed point before deciding whether it fits.
            if caps.document_bytes != bytes.len() {
                caps.document_bytes = bytes.len();
                continue;
            }
            if bytes.len() <= CAP_DOCUMENT_BYTES {
                tape.reset_without_window();
                return Ok(bytes);
            }
            if reads.is_empty() {
                // The result, effects, clocks or even invocation strings may
                // exceed the cap on their own. Do not emit an apparently
                // complete empty effect set or pretend the engine refused.
                let omitted_effects = tape.effects.len();
                let omitted_clocks = tape.clocks.len();
                let original = serde_json::to_vec(&outcome)
                    .map_err(|error| format!("the engine outcome cannot be serialized: {error}"))?;
                let reason = format!(
                    "the recorder document exceeds the {CAP_DOCUMENT_BYTES}-byte cap; \
                     engine status={} exit={}; engine outcome sha256={}; \
                     document truncated after {} rows; effects omitted={omitted_effects}; \
                     clocks omitted={omitted_clocks}; result_omitted={}",
                    outcome.status,
                    outcome.exit_code,
                    sha256_hex(&original),
                    reads.len() + omitted_effects + omitted_clocks,
                    result.is_none(),
                );
                let document = apparatus_document(
                    &config.case,
                    Some(config.manifest_sha256.as_str()),
                    &reason,
                );
                tape.reset_without_window();
                return Ok(document);
            }
            // Remove a deterministic tail in batches: serializing the whole
            // 8 MiB document once per dropped row is quadratic at the row cap.
            let keep = reads.len() / 2;
            let dropped_count = reads.len() - keep;
            let dropped_processes = reads[keep..]
                .iter()
                .filter(|row| matches!(row.kind, "process-row" | "process-observation"))
                .count();
            reads.truncate(keep);
            caps.process_rows = caps.process_rows.saturating_sub(dropped_processes);
            caps.dropped_rows = caps.dropped_rows.saturating_add(dropped_count as u64);
            caps.document_truncated = true;
            caps.read_rows = reads.len();
            caps.document_bytes = 0;
        }
    })
}

/// Record one leaf observation on this thread's window, or nothing when no
/// window is open. Epochs are assigned here, in row order; occurrences are
/// counted per identity key, and a key past its cap marks truncation.
fn record(
    kind: &'static str,
    site: &'static str,
    identity: Identity,
    outcome: Outcome,
    cap: Option<u64>,
) {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        if tape.reads.len() >= CAP_READ_ROWS {
            tape.read_rows_exhausted = true;
            tape.dropped_rows = tape.dropped_rows.saturating_add(1);
            return;
        }
        if matches!(kind, "process-row" | "process-observation")
            && tape.process_rows >= CAP_PROCESS_ROWS
        {
            tape.process_rows_exhausted = true;
            tape.dropped_rows = tape.dropped_rows.saturating_add(1);
            return;
        }
        let key = (kind, site, identity.clone());
        let slot = tape.occurrences.entry(key).or_insert(0);
        if *slot >= CAP_OCCURRENCES {
            tape.occurrences_exhausted = true;
            tape.dropped_rows = tape.dropped_rows.saturating_add(1);
            return;
        }
        *slot = slot.saturating_add(1);
        let occurrence = *slot;
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        if matches!(kind, "process-row" | "process-observation") {
            tape.process_rows = tape.process_rows.saturating_add(1);
        }
        let observation_proven = observation_proven(kind, &identity, &outcome);
        tape.reads.push(ReadRow {
            kind,
            site,
            identity,
            outcome,
            observation_proven,
            cap,
            epoch,
            occurrence,
        });
    });
}

/// Only families with an explicit, observed identity can satisfy an obligation.
/// Unknown families stay unproven until their source sites have been audited.
fn observation_proven(kind: &str, identity: &Identity, outcome: &Outcome) -> bool {
    if matches!(outcome, Outcome::Refused { .. }) {
        return false;
    }
    match kind {
        "child-stat" if matches!(outcome, Outcome::Absent) => {
            identity.parent_device.is_some()
                && identity.parent_inode.is_some()
                && identity.component.is_some()
        }
        "child-stat" if matches!(outcome, Outcome::Facts { .. }) => {
            identity.parent_device.is_some()
                && identity.parent_inode.is_some()
                && identity.component.is_some()
                && identity.device.is_some()
                && identity.inode.is_some()
        }
        "file" if matches!(outcome, Outcome::Digest { .. }) => {
            identity.parent_device.is_some()
                && identity.parent_inode.is_some()
                && identity.device.is_some()
                && identity.inode.is_some()
        }
        "process-row" | "process-observation" | "platform-status" => {
            identity.pid.is_some() && identity.start.is_some() && identity.uid.is_some()
        }
        "fd-list"
            if matches!(
                outcome,
                Outcome::Listed {
                    truncated: false,
                    ..
                }
            ) =>
        {
            identity.pid.is_some() && identity.start.is_some() && identity.uid.is_some()
        }
        "fd-entry" if matches!(outcome, Outcome::Answer { .. }) => {
            identity.pid.is_some()
                && identity.start.is_some()
                && identity.uid.is_some()
                && identity.fd.is_some()
        }
        "uid" | "boot" => matches!(outcome, Outcome::Answer { .. }),
        "process-table" => matches!(
            outcome,
            Outcome::Listed {
                truncated: false,
                ..
            }
        ),
        _ => false,
    }
}

/// The direct-read site inventory (K2/K3/K4): a path precheck stat, a bounded
/// path read or a path enumeration. None of these operations holds the parent
/// descriptor, so a row claiming a `handle` parent at one of them is
/// provenance laundering, not an observation.
pub(crate) const DIRECT_READ_SITES: &[&str] = &[
    "state::read_bounded_inner",
    "registry::read",
    "MacOs::path_facts",
    "area::read_directory",
    "area::read_holds",
    "reference::read_receipts_with",
    "state::read_intents_with_deadline",
];

/// The sites whose observation is the held directory itself (R8).
pub(crate) const HANDLE_SELF_SITES: &[&str] = &["MacOs::dir_facts", "MacOs::recheck_dir"];

/// PCF-4 control: a direct-read site cannot have observed a parent descriptor,
/// so such a row would launder its provenance. The control names the rows, it
/// does not repair them.
pub(crate) fn laundered_parent_rows(rows: &[ReadRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| {
            row.identity.parent_source == Provenance::Handle
                && DIRECT_READ_SITES.contains(&row.site)
        })
        .map(|row| format!("{} claims a handle parent at a direct-read site", row.site))
        .collect()
}

/// PCF-5 control: a handle self-observation whose own identity differs from the
/// context identity of the handle it was taken from is an incoherent reread
/// inside one window.
pub(crate) fn incoherent_handle_rereads(rows: &[ReadRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| HANDLE_SELF_SITES.contains(&row.site))
        .filter(|row| {
            row.identity
                .device
                .is_some_and(|device| row.identity.parent_device != Some(device))
                || row
                    .identity
                    .inode
                    .is_some_and(|inode| row.identity.parent_inode != Some(inode))
        })
        .map(|row| {
            format!(
                "{} observed a directory identity that differs from its handle context",
                row.site
            )
        })
        .collect()
}

/// Record one clock reading on this thread's window.
fn clock(kind: &'static str, site: String, value_ms: u64) {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        tape.clocks.push(ClockRow {
            kind,
            site,
            value_ms,
            epoch,
        });
    });
}

/// Record one effect row on this thread's window.
#[track_caller]
fn effect(kind: &'static str, path: &Path) {
    let caller = std::panic::Location::caller();
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        tape.effects.push(EffectRow {
            kind,
            site: format!("{}:{}", caller.file(), caller.line()),
            path: path.to_str().map(str::to_owned),
            match_path: path.to_path_buf(),
            parent_device: None,
            parent_inode: None,
            device: None,
            inode: None,
            destination: None,
            outcome: None,
            partial_effect_possible: false,
            epoch,
        });
    });
}

/// Bind an attempt to its already-open parent and, for rename, its destination.
#[track_caller]
pub(crate) fn child_effect(
    kind: &'static str,
    parent: (u64, u64),
    name: &str,
    destination: Option<&str>,
) {
    let caller = std::panic::Location::caller();
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        tape.effects.push(EffectRow {
            kind,
            site: format!("{}:{}", caller.file(), caller.line()),
            path: Some(name.to_owned()),
            match_path: PathBuf::from(name),
            parent_device: Some(parent.0),
            parent_inode: Some(parent.1),
            device: None,
            inode: None,
            destination: destination.map(str::to_owned),
            outcome: None,
            partial_effect_possible: false,
            epoch,
        });
    });
}

/// Record a sync result separately from the effect set; sync is not a write effect.
pub(crate) fn sync_observation(
    site: &'static str,
    directory: (u64, u64),
    component: Option<&str>,
    error: Option<&std::io::Error>,
) {
    let identity = match component {
        Some(name) => Identity::component(Some(directory), name),
        None => Identity {
            device: Some(directory.0),
            inode: Some(directory.1),
            own_source: Provenance::Handle,
            ..Identity::none()
        },
    };
    let outcome = error.map_or_else(
        || Outcome::answer("succeeded"),
        |error| Outcome::refused(errno_class(error)),
    );
    record("sync", site, identity, outcome, None);
}

/// Record a sync on a path whose opened descriptor identity was not observed.
pub(crate) fn sync_path(site: &'static str, path: &Path, error: Option<&std::io::Error>) {
    let outcome = error.map_or_else(
        || Outcome::answer("succeeded"),
        |error| Outcome::refused(errno_class(error)),
    );
    record("sync", site, Identity::file(path, None), outcome, None);
}

/// Bind the syscall result to its existing attempt row, without a second effect.
pub(crate) fn effect_outcome(kind: &str, path: &Path, error: Option<&std::io::Error>) {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if let Some(row) = tape
            .effects
            .iter_mut()
            .rev()
            .find(|row| row.kind == kind && row.match_path == path && row.outcome.is_none())
        {
            row.outcome = Some(error.map_or_else(|| "succeeded".to_owned(), errno_class));
            row.partial_effect_possible = error.is_some();
        }
    });
}

/// The io error's errno class, as the transcript binds it.
pub(crate) fn errno_class(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EPERM) => "EPERM".to_owned(),
        Some(libc::ENOENT) => "ENOENT".to_owned(),
        Some(libc::ESRCH) => "ESRCH".to_owned(),
        Some(libc::EBADF) => "EBADF".to_owned(),
        Some(libc::EACCES) => "EACCES".to_owned(),
        Some(libc::ENOTDIR) => "ENOTDIR".to_owned(),
        Some(libc::ENAMETOOLONG) => "ENAMETOOLONG".to_owned(),
        Some(libc::EIO) => "EIO".to_owned(),
        Some(libc::ENODEV) => "ENODEV".to_owned(),
        Some(libc::ENXIO) => "ENXIO".to_owned(),
        Some(libc::ELOOP) => "ELOOP".to_owned(),
        Some(libc::EEXIST) => "EEXIST".to_owned(),
        Some(libc::ENOTEMPTY) => "ENOTEMPTY".to_owned(),
        Some(libc::EXDEV) => "EXDEV".to_owned(),
        Some(libc::ENOSPC) => "ENOSPC".to_owned(),
        Some(errno) => format!("errno_{errno}"),
        None => error.kind().to_string(),
    }
}

/// Record a read of regular bytes: digest, size and the cap that bound them.
pub(crate) fn read_present(
    site: &'static str,
    path: &Path,
    facts: Option<(u64, u64)>,
    bytes: &[u8],
    cap: Option<u64>,
) {
    record(
        "file",
        site,
        Identity::file(path, facts),
        Outcome::digest(bytes, cap),
        cap,
    );
}

/// Record a component read bound to its observed parent: digest with cap.
pub(crate) fn read_present_component(
    site: &'static str,
    parent: (u64, u64),
    name: &str,
    bytes: &[u8],
    cap: Option<u64>,
) {
    let mut identity = Identity::component(Some(parent), name);
    identity.parent_source = Provenance::Handle;
    record("file", site, identity, Outcome::digest(bytes, cap), cap);
}

/// Record bytes read from a descriptor whose parent and own facts were checked.
pub(crate) fn read_present_component_bound(
    site: &'static str,
    parent: (u64, u64),
    name: &str,
    own: (u64, u64),
    descriptor: i32,
    bytes: &[u8],
    cap: Option<u64>,
) {
    let mut identity = Identity::component(Some(parent), name);
    identity.parent_source = Provenance::Handle;
    identity.device = Some(own.0);
    identity.inode = Some(own.1);
    identity.fd = Some(descriptor);
    identity.own_source = Provenance::Handle;
    record("file", site, identity, Outcome::digest(bytes, cap), cap);
}

pub(crate) fn read_child_refused(site: &'static str, parent: (u64, u64), name: &str, class: &str) {
    let mut identity = Identity::component(Some(parent), name);
    identity.parent_source = Provenance::Handle;
    record("file", site, identity, Outcome::refused(class), None);
}

/// Record a positive absence at a path.
pub(crate) fn read_absent(site: &'static str, path: &Path) {
    record(
        "file",
        site,
        Identity::file(path, None),
        Outcome::absent(),
        None,
    );
}

/// Record a refused read at a path.
pub(crate) fn read_refused(site: &'static str, path: &Path, class: &str) {
    record(
        "file",
        site,
        Identity::file(path, None),
        Outcome::refused(class),
        None,
    );
}

/// Record a stat: present facts, explicit absence, or a refusal class.
pub(crate) fn stat(site: &'static str, path: &Path, observed: Option<(u64, u64, &'static str)>) {
    let (identity, outcome) = match observed {
        Some((device, inode, kind)) => (
            Identity::file(path, Some((device, inode))),
            Outcome::facts(kind),
        ),
        None => (Identity::file(path, None), Outcome::absent()),
    };
    record("stat", site, identity, outcome, None);
}

/// Record a refused stat.
pub(crate) fn stat_refused(site: &'static str, path: &Path, class: &str) {
    record(
        "stat",
        site,
        Identity::file(path, None),
        Outcome::refused(class),
        None,
    );
}

/// Record an `io::Result`-shaped stat (the injected control-directory lookup).
pub(crate) fn io_stat(
    site: &'static str,
    path: &Path,
    observed: &Result<Metadata, std::io::Error>,
) {
    match observed {
        Ok(metadata) => {
            let kind = if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };
            stat(site, path, Some((metadata.dev(), metadata.ino(), kind)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => stat(site, path, None),
        Err(error) => stat_refused(site, path, &errno_class(error)),
    }
}

/// Record one entry seen in an enumeration, bound to the observed parent.
pub(crate) fn dir_entry(site: &'static str, parent: Option<(u64, u64)>, name: &std::ffi::OsStr) {
    dir_entry_with_source(site, parent, name, Provenance::Context);
}

/// Record a name obtained through a held directory descriptor.
pub(crate) fn handle_dir_entry(
    site: &'static str,
    descriptor: i32,
    parent: (u64, u64),
    name: &std::ffi::OsStr,
) {
    let mut identity = Identity::none();
    identity.fd = Some(descriptor);
    identity.parent_device = Some(parent.0);
    identity.parent_inode = Some(parent.1);
    identity.parent_source = Provenance::Handle;
    if let Some(name) = name.to_str() {
        identity.component = Some(name.to_owned());
        record("dir-entry", site, identity, Outcome::facts("entry"), None);
    } else {
        record(
            "dir-entry",
            site,
            identity,
            Outcome::refused("non_utf8_component"),
            None,
        );
    }
}

/// Record an enumeration name with an earlier path precheck as context only.
pub(crate) fn extracted_dir_entry(
    site: &'static str,
    parent: Option<(u64, u64)>,
    name: &std::ffi::OsStr,
) {
    dir_entry_with_source(site, parent, name, Provenance::Extracted);
}

fn dir_entry_with_source(
    site: &'static str,
    parent: Option<(u64, u64)>,
    name: &std::ffi::OsStr,
    source: Provenance,
) {
    match name.to_str() {
        Some(name) => {
            let mut identity = Identity::component(parent, name);
            if parent.is_some() {
                identity.parent_source = source;
            }
            record("dir-entry", site, identity, Outcome::facts("entry"), None);
        }
        None => {
            let mut identity = Identity::none();
            identity.parent_device = parent.map(|(device, _)| device);
            identity.parent_inode = parent.map(|(_, inode)| inode);
            if parent.is_some() {
                identity.parent_source = source;
            }
            record(
                "dir-entry",
                site,
                identity,
                Outcome::refused("non_utf8_component"),
                None,
            );
        }
    }
}

/// Record one `fstatat` result against the already-open parent.
pub(crate) fn child_stat(
    site: &'static str,
    parent: (u64, u64),
    name: &str,
    facts: Option<(u64, u64, &str)>,
    error: Option<&str>,
) {
    child_stat_inner(site, None, parent, name, facts, error);
}

pub(crate) fn child_stat_on(
    site: &'static str,
    descriptor: i32,
    parent: (u64, u64),
    name: &str,
    facts: Option<(u64, u64, &str)>,
    error: Option<&str>,
) {
    child_stat_inner(site, Some(descriptor), parent, name, facts, error);
}

fn child_stat_inner(
    site: &'static str,
    descriptor: Option<i32>,
    parent: (u64, u64),
    name: &str,
    facts: Option<(u64, u64, &str)>,
    error: Option<&str>,
) {
    let mut identity = Identity::component(Some(parent), name);
    identity.parent_source = Provenance::Handle;
    identity.fd = descriptor;
    let outcome = if let Some(error) = error {
        Outcome::refused(error)
    } else if let Some((device, inode, kind)) = facts {
        identity.device = Some(device);
        identity.inode = Some(inode);
        identity.own_source = Provenance::Extracted;
        Outcome::facts(kind)
    } else {
        Outcome::absent()
    };
    record("child-stat", site, identity, outcome, None);
}

/// Record an enumeration call's outcome.
pub(crate) fn listing(site: &'static str, path: &Path, total: usize, kept: usize, truncated: bool) {
    record(
        "dir-list",
        site,
        Identity::file(path, None),
        Outcome::listed(total, kept, truncated),
        None,
    );
}

/// Record an observation from an already opened directory descriptor.
pub(crate) fn handle_observation(
    site: &'static str,
    kind: &'static str,
    descriptor: i32,
    device: u64,
    inode: u64,
    outcome: Outcome,
) {
    let identity = Identity {
        device: Some(device),
        inode: Some(inode),
        fd: Some(descriptor),
        own_source: Provenance::Handle,
        ..Identity::none()
    };
    record(kind, site, identity, outcome, None);
}

/// Record an observation made through an already-open descriptor, linked to the
/// context of the handle it was taken from.
///
/// S8 (`duplicate_dir`) and R8 (`dir_facts`, `recheck_dir`) observe the held
/// directory itself. The row keeps the identity the descriptor actually
/// supplied as its own (`handle`) and names the handle's opened identity as its
/// `context` parent. The shared wire has no epoch-reference field, so the link
/// is the observed identity and its provenance, not an epoch cross-reference; a
/// mandatory parent stays unproven.
pub(crate) fn linked_handle_observation(
    site: &'static str,
    kind: &'static str,
    descriptor: i32,
    own: (u64, u64),
    context: (u64, u64),
    outcome: Outcome,
) {
    let identity = Identity {
        device: Some(own.0),
        inode: Some(own.1),
        fd: Some(descriptor),
        own_source: Provenance::Handle,
        parent_device: Some(context.0),
        parent_inode: Some(context.1),
        parent_source: Provenance::Context,
        ..Identity::none()
    };
    record(kind, site, identity, outcome, None);
}

/// Record an enumeration refusal.
pub(crate) fn listing_refused(site: &'static str, path: &Path, class: &str) {
    record(
        "dir-list",
        site,
        Identity::file(path, None),
        Outcome::refused(class),
        None,
    );
}

/// Whether a lock was held, was absent, or refused.
pub(crate) enum LockOutcome {
    /// Held, with the lock file's identity when it could be read.
    Held(Option<(u64, u64)>, i32),
    /// The lock file does not exist.
    Absent,
    /// The lock could not be taken or inspected.
    Refused(String),
}

/// Record a lock observation: held with the lock file's identity when it
/// could be read, positively absent, or refused.
pub(crate) fn lock(site: &'static str, path: &Path, outcome: LockOutcome) {
    let (identity, outcome) = match outcome {
        LockOutcome::Held(facts, descriptor) => {
            let mut identity = Identity::file(path, facts);
            identity.fd = Some(descriptor);
            if facts.is_some() {
                identity.own_source = Provenance::Handle;
            }
            (identity, Outcome::facts("held"))
        }
        LockOutcome::Absent => (Identity::file(path, None), Outcome::absent()),
        LockOutcome::Refused(class) => (Identity::file(path, None), Outcome::refused(class)),
    };
    record("lock", site, identity, outcome, None);
}

/// Record the context link for an object opened by pathname where the caller
/// already holds the descriptor of the directory the object is a child of. The
/// opened object keeps its own observed identity; the parent is `context` from
/// the held handle, so a mandatory parent stays unproven and nothing is
/// inferred by a later lookup.
pub(crate) fn context_link(site: &'static str, path: &Path, own: (u64, u64), context: (u64, u64)) {
    let mut identity = Identity::file(path, Some(own));
    identity.parent_device = Some(context.0);
    identity.parent_inode = Some(context.1);
    identity.parent_source = Provenance::Context;
    record(
        "context-link",
        site,
        identity,
        Outcome::facts("opened"),
        None,
    );
}

/// Record one observed pool or lock holder. The record's own pid and start
/// reading are facts; the kernel start and uid are `null` when this run did not
/// observe them, so completeness reports the lifetime as unproven.
pub(crate) fn recorded_holder(
    site: &'static str,
    record_id: u64,
    pid: u32,
    recorded_start_ms: u64,
    kernel_start: Option<u64>,
    uid: Option<u32>,
) {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        if tape.holder_rows >= CAP_PROCESS_ROWS {
            tape.holders_exhausted = true;
            tape.dropped_rows = tape.dropped_rows.saturating_add(1);
            return;
        }
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        tape.holder_rows = tape.holder_rows.saturating_add(1);
        tape.holders.push(RecordedHolder {
            site: site.to_owned(),
            record_id,
            pid,
            recorded_start_ms,
            kernel_start,
            uid,
            epoch,
        });
    });
}

/// Record one kernel process-table row: never a pid alone.
pub(crate) fn process_row(site: &'static str, pid: u32, uid: Option<u32>, start: u64) {
    record(
        "process-row",
        site,
        Identity::process(pid, start, uid, None),
        Outcome::facts("row"),
        None,
    );
}

/// Preserve classified and retry identities as separate ordered observations.
pub(crate) fn observed_process(
    site: &'static str,
    pid: u32,
    start: u64,
    uid: Option<u32>,
    label: &str,
) {
    record(
        "process-observation",
        site,
        Identity::process(pid, start, uid, None),
        Outcome::answer(label),
        None,
    );
}

/// Record the process-table read's own outcome.
pub(crate) fn process_table(site: &'static str, outcome: Outcome) {
    record("process-table", site, Identity::none(), outcome, None);
}

/// Record one per-pid descriptor listing.
pub(crate) fn fd_list(site: &'static str, pid: u32, outcome: Outcome) {
    // A prior classification does not identify the process at this syscall.
    let identity = Identity::unidentified_process(pid, None);
    PROCESS_CONTEXT.with(|slot| *slot.borrow_mut() = None);
    record("fd-list", site, identity, outcome, None);
}

/// Record one descriptor returned by the process listing.
pub(crate) fn fd_entry(site: &'static str, pid: u32, fd: i32, fd_type: u32) {
    record(
        "fd-entry",
        site,
        Identity::unidentified_process(pid, Some(fd)),
        Outcome::answer(fd_type.to_string()),
        None,
    );
}

/// Record one per-descriptor vnode answer: `which` is `vnode-path` or
/// `vnode-facts`, and a readable path names itself.
pub(crate) fn vnode(
    site: &'static str,
    pid: u32,
    fd: i32,
    which: &'static str,
    outcome: Outcome,
    path: Option<String>,
) {
    let mut identity = Identity::unidentified_process(pid, Some(fd));
    identity.path = path;
    record(which, site, identity, outcome, None);
}

/// Record vnode facts from the syscall's returned descriptor buffer.
pub(crate) fn vnode_observed(site: &'static str, pid: u32, fd: i32, device: u64, inode: u64) {
    let mut identity = Identity::unidentified_process(pid, Some(fd));
    identity.device = Some(device);
    identity.inode = Some(inode);
    identity.own_source = Provenance::Extracted;
    record(
        "vnode-facts",
        site,
        identity,
        Outcome::answer("readable"),
        None,
    );
}

/// Record one code-signing status answer for a classified identity.
pub(crate) fn platform(site: &'static str, pid: u32, start: u64, uid: Option<u32>, answer: &str) {
    let outcome = if matches!(answer, "not_verified:gone" | "not_verified:start_changed") {
        Outcome::refused(answer)
    } else {
        Outcome::answer(answer)
    };
    record(
        "platform-status",
        site,
        Identity::process(pid, start, uid, None),
        outcome,
        None,
    );
}

/// Record a platform observation whose process identity was not bound.
pub(crate) fn platform_unidentified(site: &'static str, pid: u32, answer: &str) {
    let identity = process_identity(pid, None);
    PROCESS_CONTEXT.with(|slot| *slot.borrow_mut() = None);
    record(
        "code-signing-status",
        site,
        identity,
        Outcome::answer(answer),
        None,
    );
}

/// Record a scalar fact: the effective uid, the boot identity, a start.
pub(crate) fn scalar(site: &'static str, kind: &'static str, identity: Identity, answer: &str) {
    record(kind, site, identity, Outcome::answer(answer), None);
}

/// Record a volume identity or mount-root observation.
pub(crate) fn volume(site: &'static str, identity: Identity, outcome: Outcome) {
    record("volume-identity", site, identity, outcome, None);
}

/// Record a classified verdict at a named source site.
pub(crate) fn verdict(site: &'static str, identity: Identity, answer: &str) {
    record("verdict", site, identity, Outcome::answer(answer), None);
}

/// Record one custody record's intermediate classification.
pub(crate) fn custody_record(site: &'static str, pool: &Path, record_id: &str, class: &str) {
    let mut identity = Identity::file(pool, None);
    identity.component = Some(record_id.to_owned());
    record(
        "custody-record",
        site,
        identity,
        Outcome::answer(class),
        None,
    );
}

/// Observe the exact I/O result without changing it or repeating the operation.
#[track_caller]
pub(crate) fn io_effect<T>(
    kind: &'static str,
    path: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    write_effect(kind, path);
    let result = operation();
    effect_outcome(kind, path, result.as_ref().err());
    result
}

/// Record an attempted rename, including its destination, then bind its result.
#[track_caller]
pub(crate) fn io_rename(
    source: &Path,
    destination: &Path,
    operation: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    write_effect("rename", source);
    TAPE.with(|tape| {
        if let Some(row) = tape.borrow_mut().effects.last_mut() {
            if row.kind == "rename" && row.match_path == source && row.outcome.is_none() {
                row.destination = destination.to_str().map(str::to_owned);
            }
        }
    });
    let result = operation();
    effect_outcome("rename", source, result.as_ref().err());
    result
}

/// Record the caller clock: the value the engine passed or read.
pub(crate) fn now_ms(value_ms: u64) {
    clock("now_ms", "test".to_owned(), value_ms);
}

/// Record a clock value already stored in an evaluator result at its site.
pub(crate) fn now_ms_site(value_ms: u64, site: &'static str) {
    clock("now_ms", site.to_owned(), value_ms);
}

/// Record the single value returned to its actual call site.
pub(crate) fn now_ms_at(value_ms: u64, caller: &'static std::panic::Location<'static>) {
    clock(
        "now_ms",
        format!("{}:{}", caller.file(), caller.line()),
        value_ms,
    );
}

/// Record a real or attempted effect from the contract's closed set.
#[track_caller]
pub(crate) fn write_effect(kind: &'static str, path: &Path) {
    effect(kind, path);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn change_errno_to_enoent() {
        let error = std::fs::metadata("").expect_err("an empty path is missing");
        let current = std::io::Error::last_os_error();
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(current.raw_os_error(), Some(libc::ENOENT));
    }

    /// A `/2` manifest for the completeness controls: one Barge obligation, an
    /// explicitly declared effect set, and the schema's complete source block.
    /// Digest values are placeholders: these controls judge completeness rules,
    /// never a source binding.
    fn shared_manifest(
        case: &str,
        obligations: serde_json::Value,
        expected_effects: serde_json::Value,
    ) -> Vec<u8> {
        let manifest = serde_json::json!({
            "schema": MANIFEST_SCHEMA,
            "case": case,
            "window": {"identity": "control", "start_monotonic_ns": 1, "end_monotonic_ns": 2},
            "source": {
                "oxide_commit": "control",
                "oxide_files": {},
                "cargo_lock_sha256": "0".repeat(64),
                "rust_toolchain_sha256": "0".repeat(64),
                "barge_pin": "4af9360bd09629e970c143080ca159de8eef2562",
                "barge_slice_commit": "control",
                "barge_files": {},
                "staged_lock_sha256": "0".repeat(64),
                "oxide_toolchain": "1.97.1",
                "barge_toolchain": "1.97.1",
                "oxide_executable_sha256": "0".repeat(64),
                "barge_executable_sha256": "0".repeat(64),
                "oxide_tree": "0".repeat(40),
            },
            "fixture": {"root": "/fixture", "device": 1, "inode": 2, "mode": 448, "entries": [], "roots": []},
            "dynamic": {"volumes": [], "writers": [], "external_allowances": []},
            "mandatory_observations": obligations,
            "expected_effects": expected_effects,
            "expected": {"classification": "MATCH", "roots": []},
            "barge": {"volume_uuid": "0", "namespace": "oxide", "area": "outputs"},
        });
        serde_json::to_vec(&manifest).expect("the control manifest encodes")
    }

    fn one_obligation(
        site: &str,
        parent_required: bool,
        identity: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!([{
            "engine": "barge",
            "site": site,
            "parent_required": parent_required,
            "identity": identity,
        }])
    }

    /// Finish the open window as a completed engine result.
    fn finish_completed() -> String {
        finish(
            EngineOutcome {
                status: "completed",
                exit_code: 0,
                refusal_kind: None,
                refusal_message: None,
                refusal_detail: Vec::new(),
                engine_truncated: false,
            },
            Some(&serde_json::json!({"preview": true})),
        )
        .expect("the recorder document is written")
    }

    #[test]
    fn completeness_is_delegated_to_the_shared_rules() {
        // One proven obligation plus an explicitly empty effect set: the only
        // remaining issues are the unproven window origin and, for K2, nothing
        // else. The rules are the shared parser's, not a second projection.
        let case = "CONTROL-COMPLETE";
        begin_window(WindowConfig {
            case: case.to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        process_row("process-row-site", 42, Some(501), 123);
        let recorded = finish_completed();
        let wire = shared_transcript(&recorded).expect("the wire converts");
        let manifest = shared_manifest(
            case,
            one_obligation(
                "process-row-site#process-row",
                false,
                serde_json::json!({"pid": 42, "start": 123, "uid": 501}),
            ),
            serde_json::json!([]),
        );
        let issues = assess_shared(&wire, &manifest)
            .expect("completeness runs")
            .issues;
        assert_eq!(issues, vec!["window origin and end unproven".to_owned()]);

        // A wrong identity and a missing obligation are named, never matched.
        let wrong = shared_manifest(
            case,
            one_obligation(
                "process-row-site#process-row",
                false,
                serde_json::json!({"pid": 42, "start": 124, "uid": 501}),
            ),
            serde_json::json!([]),
        );
        let issues = assess_shared(&wire, &wrong)
            .expect("completeness runs")
            .issues;
        assert!(
            issues.contains(&"wrong identity at process-row-site#process-row".to_owned()),
            "{issues:?}"
        );
        let missing = shared_manifest(
            case,
            one_obligation(
                "never-observed",
                false,
                serde_json::json!({"pid": 42, "start": 123, "uid": 501}),
            ),
            serde_json::json!([]),
        );
        let issues = assess_shared(&wire, &missing)
            .expect("completeness runs")
            .issues;
        assert!(
            issues.contains(&"missing obligation at never-observed".to_owned()),
            "{issues:?}"
        );
    }

    #[test]
    fn a_handle_parent_stays_unproven_and_an_attempt_is_not_parity() {
        let case = "CONTROL-PARENT";
        begin_window(WindowConfig {
            case: case.to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        child_stat("child-site", (1, 2), "child", None, None);
        write_effect("save", Path::new("/fixture/ledger"));
        let recorded = finish_completed();
        let wire = shared_transcript(&recorded).expect("the wire converts");
        let manifest = shared_manifest(
            case,
            one_obligation(
                "child-site#child-stat",
                true,
                serde_json::json!({"component": "child", "parent_device": 1, "parent_inode": 2}),
            ),
            serde_json::json!([{
                "kind": "save", "site": "ledger", "path": "/fixture/ledger", "attempted": false
            }]),
        );
        let issues = assess_shared(&wire, &manifest)
            .expect("completeness runs")
            .issues;
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("parent_required unproven at child-site#child-stat")),
            "{issues:?}"
        );
        // The attempt's site is its real caller location; the declared effect at
        // `ledger` is neither completed nor matched.
        assert!(
            issues.iter().any(|issue| {
                issue.starts_with("effect not completed at ") && issue.ends_with("(unknown)")
            }),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue == "omitted or extra effects"),
            "{issues:?}"
        );
    }

    #[test]
    fn apparatus_documents_are_distinct_from_captures() {
        // A completed window whose report cannot be carried keeps its real
        // engine status in a distinct apparatus diagnostic.
        begin_window(WindowConfig {
            case: "CONTROL-APPARATUS".to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        let document = finish_apparatus(
            "the engine completed with exit code 3 but its report could not be serialized",
        )
        .expect("the apparatus diagnostic is written");
        let value: serde_json::Value = serde_json::from_str(&document).expect("JSON");
        assert_eq!(value["schema"], APPARATUS_SCHEMA);
        assert_eq!(value["version"], APPARATUS_SCHEMA);
        assert_eq!(value["case"], "CONTROL-APPARATUS");
        assert!(value["reason"]
            .as_str()
            .expect("a reason")
            .contains("exit code 3"));
        // The transcript parser refuses it by name, so it cannot read as a capture.
        let refused = capture_wire::parse_transcript(document.as_bytes()).expect_err("refused");
        assert!(refused.message.contains("unknown version"), "{refused:?}");
        // One engine outcome per window: a second finish has nothing to close.
        assert!(finish_apparatus("again").is_err());
    }

    /// Open a window, run `body`, and return the finished transcript string.
    fn window(body: impl FnOnce()) -> String {
        begin_window(WindowConfig {
            case: "CTL-0".to_owned(),
            manifest_sha256: "m".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "test".to_owned(),
        })
        .expect("the window opens");
        body();
        finish(
            EngineOutcome {
                status: "completed",
                exit_code: 0,
                refusal_kind: None,
                refusal_message: None,
                refusal_detail: Vec::new(),
                engine_truncated: false,
            },
            Some(&serde_json::json!({"preview": true})),
        )
        .expect("the transcript is written")
    }

    #[test]
    fn published_capture_vectors_pass_their_controls() {
        super::capture_wire::run_self_checks().expect("published capture vectors");
    }

    #[test]
    fn shared_wire_preserves_actual_row_and_engine_result() {
        let transcript = window(|| {
            child_stat("stat", (1, 2), "missing", None, None);
        });
        let bytes = shared_transcript(&transcript).expect("wire conversion");
        let document = capture_wire::parse_transcript(bytes.as_bytes()).expect("wire parse");
        assert_eq!(document["reads"][0]["identity"]["parent_source"], "handle");
        assert_eq!(document["reads"][0]["outcome"]["type"], "absent");
        assert_eq!(document["outcome"]["state"], "completed");
        assert_eq!(document["outcome"]["result"]["preview"], true);
    }

    #[test]
    fn held_parent_rows_survive_rename_and_refuse_a_symlink_open() {
        use crate::os::DirOps;

        let fixture = crate::testkit::TempDir::under_scratch("tape-held-parent");
        let before = fixture.path().join("before");
        let after = fixture.path().join("after");
        std::fs::create_dir(&before).expect("parent directory");
        std::fs::create_dir(before.join("child")).expect("child directory");
        std::os::unix::fs::symlink("child", before.join("link")).expect("fixture link");
        let dirs = crate::os::platform();
        let parent = dirs.open_path(&before).expect("open parent");
        let transcript = window(|| {
            std::fs::rename(&before, &after).expect("rename held parent");
            dirs.open_child_dir(&parent, "child").expect("held child");
            assert!(dirs
                .child_facts(&parent, "missing")
                .expect("absence")
                .is_none());
            assert!(dirs.open_child_dir(&parent, "link").is_err());
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("JSON");
        let rows = document["reads"].as_array().expect("rows");
        for name in ["child", "missing", "link"] {
            assert!(rows.iter().any(|row| {
                row["identity"]["component"] == name && row["identity"]["parent_source"] == "handle"
            }));
        }
        assert!(rows.iter().any(|row| {
            row["identity"]["component"] == "link" && row["outcome"].get("refused").is_some()
        }));
    }

    #[test]
    fn a_window_records_rows_and_resets() {
        let transcript = window(|| {
            read_present(
                "state::read_bounded_inner",
                Path::new("/fixture/registry.json"),
                Some((1, 2)),
                b"bytes",
                Some(64),
            );
            read_absent("state::read_bounded_inner", Path::new("/fixture/gone.json"));
            now_ms(1_792_525_019_847);
        });
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["schema"], RECORDER_SCHEMA);
        assert_eq!(document["engine"], ENGINE);
        assert_eq!(document["case"], "CTL-0");
        assert_eq!(document["reads"].as_array().map(Vec::len), Some(2));
        assert_eq!(document["clocks"].as_array().map(Vec::len), Some(1));
        assert_eq!(document["effects"].as_array().map(Vec::len), Some(0));
        let row = &document["reads"][0];
        assert_eq!(row["site"], "state::read_bounded_inner");
        assert_eq!(row["identity"]["path"], "/fixture/registry.json");
        assert_eq!(row["identity"]["device"], 1);
        assert_eq!(row["identity"]["inode"], 2);
        assert_eq!(
            row["outcome"]["digest"]["bytes_sha256"],
            sha256_hex(b"bytes")
        );
        assert_eq!(row["cap"], 64);
        assert_eq!(row["epoch"], 1);
        assert_eq!(row["occurrence"], 1);
        assert_eq!(document["caps"]["read_rows"], 2);
        // A second identical observation is occurrence 2 of the same key.
        let again = window(|| {
            read_present(
                "state::read_bounded_inner",
                Path::new("/fixture/registry.json"),
                Some((1, 2)),
                b"bytes",
                Some(64),
            );
            read_present(
                "state::read_bounded_inner",
                Path::new("/fixture/registry.json"),
                Some((1, 2)),
                b"bytes",
                Some(64),
            );
        });
        let document: serde_json::Value =
            serde_json::from_str(&again).expect("the transcript parses");
        assert_eq!(document["reads"][1]["occurrence"], 2);
    }

    #[test]
    fn the_refusal_document_carries_the_refusal_and_no_result() {
        begin_window(WindowConfig {
            case: "REG-MISSING".to_owned(),
            manifest_sha256: "m".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "test".to_owned(),
        })
        .expect("the window opens");
        read_absent("registry::read", Path::new("/fixture/barge.registry/1"));
        let transcript = finish(
            EngineOutcome {
                status: "refused",
                exit_code: 3,
                refusal_kind: Some("unknown_state".to_owned()),
                refusal_message: Some("volume 0 is not registered".to_owned()),
                refusal_detail: vec![("volume_uuid".to_owned(), serde_json::json!("0"))],
                engine_truncated: false,
            },
            None,
        )
        .expect("the refusal document is written");
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["outcome"]["status"], "refused");
        assert_eq!(document["outcome"]["exit_code"], 3);
        assert_eq!(document["outcome"]["refusal_kind"], "unknown_state");
        assert!(document.get("result").is_none());
        assert_eq!(document["reads"][0]["outcome"], "absent");
    }

    #[test]
    fn a_refused_window_rejects_a_result_and_versa() {
        begin_window(WindowConfig {
            case: "c".to_owned(),
            manifest_sha256: "m".repeat(64),
            entrypoint_symbol: "s",
            source_binding: "t".to_owned(),
        })
        .expect("the window opens");
        let refused = finish(
            EngineOutcome {
                status: "refused",
                exit_code: 2,
                refusal_kind: None,
                refusal_message: None,
                refusal_detail: Vec::new(),
                engine_truncated: false,
            },
            Some(&serde_json::json!({})),
        );
        assert!(refused.is_err(), "a refusal carries no result");
        // The window was consumed by the failed close; reopen for the second half.
        begin_window(WindowConfig {
            case: "c".to_owned(),
            manifest_sha256: "m".repeat(64),
            entrypoint_symbol: "s",
            source_binding: "t".to_owned(),
        })
        .expect("the window reopens");
        let completed = finish(
            EngineOutcome {
                status: "completed",
                exit_code: 0,
                refusal_kind: None,
                refusal_message: None,
                refusal_detail: Vec::new(),
                engine_truncated: false,
            },
            None,
        );
        assert!(completed.is_err(), "a completion carries the result");
    }

    #[test]
    fn the_read_row_cap_truncates_and_never_quiets() {
        let transcript = window(|| {
            for index in 0..(CAP_READ_ROWS + 8) {
                let path = PathBuf::from(format!("/fixture/file-{index}"));
                record(
                    "file",
                    "state::read_bounded_inner",
                    Identity::file(&path, None),
                    Outcome::absent(),
                    None,
                );
            }
        });
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["caps"]["read_rows"], CAP_READ_ROWS);
        assert_eq!(document["caps"]["read_rows_exhausted"], true);
        assert_eq!(document["caps"]["dropped_rows"], 8);
    }

    #[test]
    fn the_occurrence_cap_truncates_per_key() {
        let transcript = window(|| {
            for _ in 0..(CAP_OCCURRENCES + 2) {
                record(
                    "file",
                    "state::read_bounded_inner",
                    Identity::file(Path::new("/fixture/one"), None),
                    Outcome::absent(),
                    None,
                );
            }
        });
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["caps"]["occurrences_exhausted"], true);
        assert_eq!(document["caps"]["dropped_rows"], 2);
    }

    #[test]
    fn the_document_cap_drops_tail_rows_and_records_truncation() {
        let filler = vec![b'x'; 512];
        // Long bound paths make each row about a kilobyte, so the full row set
        // exceeds the 8 MiB document cap and the serializer drops from the tail.
        let bound = format!("/fixture/{}", "c".repeat(900));
        begin_window(WindowConfig {
            case: "c".to_owned(),
            manifest_sha256: "m".repeat(64),
            entrypoint_symbol: "s",
            source_binding: "t".to_owned(),
        })
        .expect("the window opens");
        for index in 0..CAP_READ_ROWS - 1 {
            let path = PathBuf::from(format!("{bound}-{index}"));
            record(
                "file",
                "state::read_bounded_inner",
                Identity::file(&path, None),
                Outcome::digest(&filler, None),
                None,
            );
        }
        process_row("cap-test", 42, Some(501), 123);
        let transcript = finish(
            EngineOutcome {
                status: "completed",
                exit_code: 0,
                refusal_kind: None,
                refusal_message: None,
                refusal_detail: Vec::new(),
                engine_truncated: false,
            },
            Some(&serde_json::json!({})),
        )
        .expect("the transcript is written");
        assert!(transcript.len() <= CAP_DOCUMENT_BYTES);
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["caps"]["document_truncated"], true);
        assert_eq!(document["caps"]["document_bytes"], transcript.len());
        assert_eq!(document["caps"]["process_rows"], 0);
        assert!(document["caps"]["dropped_rows"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn oversized_non_read_payload_yields_bounded_apparatus_refusal() {
        let oversized = "x".repeat(CAP_DOCUMENT_BYTES + 1);
        let document = window(|| write_effect("create", Path::new(&oversized)));
        assert!(document.len() <= CAP_DOCUMENT_BYTES);
        let value: serde_json::Value = serde_json::from_str(&document).expect("JSON");
        // The payload cannot be a conforming capture, so the result is the
        // distinct apparatus diagnostic, never a transcript-shaped invention.
        assert_eq!(value["schema"], APPARATUS_SCHEMA);
        let reason = value["reason"].as_str().expect("a reason");
        assert!(reason.contains("exceeds the"), "{reason}");
        assert!(
            reason.contains("engine status=completed exit=0"),
            "{reason}"
        );
        assert!(reason.contains("effects omitted=1"), "{reason}");
        assert!(reason.contains("result_omitted=false"), "{reason}");
        assert_eq!(value["case"], "CTL-0");
        assert!(capture_wire::parse_transcript(document.as_bytes()).is_err());
    }

    #[test]
    fn an_identity_without_its_parent_still_binds_the_path() {
        let transcript = window(|| {
            read_absent("registry::read", Path::new("/fixture/barge.registry/1"));
        });
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        let identity = &document["reads"][0]["identity"];
        assert!(identity["parent_device"].is_null() || identity["parent_device"] == 0);
        assert_eq!(identity["path"], "/fixture/barge.registry/1");
    }

    #[test]
    fn serialization_is_deterministic_for_the_same_window() {
        let body = || {
            read_present(
                "registry::read",
                Path::new("/fixture/r"),
                Some((3, 4)),
                b"same",
                Some(10),
            );
            now_ms(7);
        };
        let first = window(body);
        let second = window(body);
        // The window's elapsed nanoseconds are the one measured field; every
        // row, row order and epoch is byte-identical between the two windows.
        let normalize = |document: &str| {
            let mut value: serde_json::Value = serde_json::from_str(document).expect("JSON");
            value["window_elapsed_ns"] = serde_json::json!(0);
            value["caps"]["document_bytes"] = serde_json::json!(0);
            serde_json::to_string(&value).expect("JSON")
        };
        assert_eq!(normalize(&first), normalize(&second));
    }

    /// Two observations of one identity key are two bound occurrences, never
    /// collapsed: an ABA change inside a window is visible as occurrence 1
    /// differing from occurrence 2 (different digests under one key), a
    /// changed inode is a different identity key from the first two, and
    /// epochs stay strictly increasing, so the reconciler can reject an
    /// inconsistent reread by key.
    #[test]
    fn a_conflicting_reread_is_two_bound_occurrences_with_its_own_outcomes() {
        let transcript = window(|| {
            let path = Path::new("/fixture/marker");
            read_present(
                "reference::read_oxide_marker",
                path,
                Some((7, 8)),
                b"first",
                Some(9),
            );
            read_present(
                "reference::read_oxide_marker",
                path,
                Some((7, 8)),
                b"second",
                Some(9),
            );
            read_present(
                "reference::read_oxide_marker",
                path,
                Some((7, 9)),
                b"second",
                Some(9),
            );
        });
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        let rows = document["reads"].as_array().expect("read rows");
        assert_eq!(rows.len(), 3);
        // The same identity twice is occurrence 1 then 2, with each read's own
        // outcome: an inconsistent reread is visible, never collapsed.
        assert_eq!(rows[0]["occurrence"], 1);
        assert_eq!(rows[1]["occurrence"], 2);
        assert_eq!(rows[0]["identity"], rows[1]["identity"]);
        assert_eq!(
            rows[0]["outcome"]["digest"]["bytes_sha256"],
            sha256_hex(b"first")
        );
        assert_eq!(
            rows[1]["outcome"]["digest"]["bytes_sha256"],
            sha256_hex(b"second")
        );
        // A changed inode is a different identity key from the first two.
        assert_eq!(rows[2]["occurrence"], 1);
        assert_ne!(rows[0]["identity"], rows[2]["identity"]);
        let epochs: Vec<u64> = rows
            .iter()
            .filter_map(|row| row["epoch"].as_u64())
            .collect();
        assert!(
            epochs.windows(2).all(|pair| pair[0] < pair[1]),
            "epochs are monotonic"
        );
    }

    /// The transcript names every fixed cap, so an exhausted bound is always
    /// visible in the document.
    #[test]
    fn the_caps_block_names_every_contract_cap() {
        let transcript = window(|| {});
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        let caps = &document["caps"];
        assert_eq!(caps["occurrences_per_key"], CAP_OCCURRENCES);
        assert_eq!(caps["process_rows"], 0);
        assert_eq!(caps["read_rows"], 0);
        assert_eq!(caps["document_bytes"], transcript.len());
        // The document cap is enforced at serialization; its bound is stated
        // here as the constant the serializer uses.
        assert_eq!(CAP_DOCUMENT_BYTES, 8 * 1024 * 1024);
        assert_eq!(CAP_PROCESS_ROWS, 512);
    }

    #[test]
    fn missing_and_replaced_parent_are_not_inferred_from_a_path() {
        let transcript = window(|| {
            read_present(
                "first",
                Path::new("/fixture/root/item"),
                Some((1, 2)),
                b"a",
                None,
            );
            read_present(
                "second",
                Path::new("/fixture/root/item"),
                Some((1, 3)),
                b"b",
                None,
            );
            dir_entry("component", None, std::ffi::OsStr::new("item"));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        for row in document["reads"].as_array().expect("reads") {
            assert_eq!(row["observation_proven"], false);
            assert!(row["identity"].get("parent_inode").is_none());
        }
        assert_ne!(
            document["reads"][0]["identity"]["inode"],
            document["reads"][1]["identity"]["inode"]
        );
        assert_eq!(document["outcome"]["status"], "completed");
        assert_eq!(document["observation_complete"], false);
    }

    #[test]
    fn occurrences_do_not_merge_delimiter_containing_identities() {
        let transcript = window(|| {
            let first = Identity::file(Path::new("a|component=b"), None);
            let mut second = Identity::file(Path::new("a"), None);
            second.component = Some("b".to_owned());
            record("file", "site", first, Outcome::answer("one"), None);
            record("file", "site", second, Outcome::answer("two"), None);
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["reads"][0]["occurrence"], 1);
        assert_eq!(document["reads"][1]["occurrence"], 1);
    }

    #[test]
    fn recorder_does_not_reopen_or_stat_a_parent() {
        let source = include_str!("tape.rs");
        let marker = "\n#[cfg(test)]\n#[allow(clippy::unwrap_used";
        let implementation = source.split_once(marker).expect("test module boundary").0;
        assert!(
            implementation.contains("fn effect("),
            "the guard must inspect the recorder implementation"
        );
        // The recorder proper takes no filesystem read of its own: the only
        // reads are the declared fixture-capture apparatus below this marker.
        let recorder = implementation
            .split_once("// ---- fixture-capture apparatus ----")
            .expect("the fixture-capture section is declared")
            .0;
        assert!(!recorder.contains("symlink_metadata"));
        assert!(!recorder.contains("read_dir"));
        assert!(!recorder.contains("File::open"));
        assert!(!implementation.contains("fn parent_identity"));
        let state = include_str!("state.rs");
        let area = include_str!("area.rs");
        assert!(!state.contains("fn lock_file_facts("));
        assert!(!state.contains("fn dir_facts(path:"));
        assert!(!area.contains("fn dir_facts(path:"));
    }

    #[test]
    fn held_lock_without_observed_descriptor_identity_stays_unproven() {
        let transcript = window(|| {
            lock(
                "state::StateLock",
                Path::new("lock"),
                LockOutcome::Held(None, 7),
            )
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["reads"][0];
        assert_eq!(row["observation_proven"], false);
        assert!(row["identity"].get("inode").is_none());
    }

    #[test]
    fn injected_l1_failure_keeps_the_real_shared_lock_held() {
        let fixture = crate::testkit::TempDir::under_scratch("tape-lock-metadata");
        let path = fixture.path().join("lock");
        std::fs::write(&path, b"").expect("lock fixture");
        let transcript = window(|| {
            crate::state::fail_next_lock_metadata();
            let lock = crate::state::StateLock::shared_if_present(
                &path,
                crate::state::LockRank::Registry,
                crate::state::Deadline::after(std::time::Duration::from_secs(2)),
            )
            .expect("the shared acquisition succeeds")
            .expect("the lock is held");
            assert_eq!(lock.mode(), crate::state::LockMode::Shared);
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("JSON");
        let rows = document["reads"].as_array().expect("rows");
        assert!(rows.iter().any(|row| {
            row["site"] == "state::StateLock::metadata"
                && row["outcome"]["refused"]["class"] == "EIO"
        }));
        assert!(rows.iter().any(|row| {
            row["site"] == "state::StateLock::shared_if_present"
                && row["outcome"]["facts"]["kind"] == "held"
                && row["identity"].get("inode").is_none()
                && row["observation_proven"] == false
        }));
    }

    #[test]
    fn injected_dup_failure_keeps_parent_open_and_allows_a_later_dup() {
        use crate::os::DirOps;

        let fixture = crate::testkit::TempDir::under_scratch("tape-dup-failure");
        let dirs = crate::os::platform();
        let parent = dirs.open_path(fixture.path()).expect("open parent");
        let transcript = window(|| {
            fail_next_dup();
            assert!(dirs.duplicate_dir(&parent).is_err());
            drop(dirs.duplicate_dir(&parent).expect("parent still open"));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("JSON");
        let rows = document["reads"].as_array().expect("rows");
        assert!(rows.iter().any(|row| {
            row["site"] == "MacOs::duplicate_dir" && row["outcome"]["refused"]["class"] == "EBADF"
        }));
        assert!(rows.iter().any(|row| {
            row["site"] == "MacOs::duplicate_dir" && row["outcome"]["facts"]["kind"] == "duplicated"
        }));
    }

    #[test]
    fn stored_absence_clock_keeps_its_site_and_value() {
        let transcript = window(|| now_ms_site(1234, "os::prove_absence_inner"));
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["clocks"][0]["site"], "os::prove_absence_inner");
        assert_eq!(document["clocks"][0]["value_ms"], 1234);
    }

    #[test]
    fn descriptor_listing_does_not_borrow_a_classification_identity() {
        let transcript = window(|| {
            process_context(42, 123, Some(501));
            fd_list("list", 42, Outcome::listed(1, 1, false));
            fd_list("list", 42, Outcome::listed(0, 0, false));
            process_context(42, 124, Some(501));
            vnode(
                "vnode",
                43,
                5,
                "vnode-facts",
                Outcome::answer("readable"),
                None,
            );
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert!(document["reads"][0]["identity"].get("start").is_none());
        assert_eq!(document["reads"][0]["observation_proven"], false);
        assert!(document["reads"][1]["identity"].get("start").is_none());
        assert_eq!(document["reads"][1]["observation_proven"], false);
        assert!(document["reads"][2]["identity"].get("start").is_none());
    }

    #[test]
    fn changed_retry_identity_does_not_relabel_later_vnode() {
        let transcript = window(|| {
            observed_process("settle", 42, 10, Some(501), "classified");
            observed_process("settle", 42, 11, Some(501), "retry_identity");
            vnode(
                "vnode",
                42,
                3,
                "vnode-facts",
                Outcome::answer("readable"),
                None,
            );
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let rows = document["reads"].as_array().expect("reads");
        assert_eq!(rows[0]["identity"]["start"], 10);
        assert_eq!(rows[1]["identity"]["start"], 11);
        assert_eq!(rows[0]["observation_proven"], true);
        assert_eq!(rows[1]["observation_proven"], true);
        assert!(rows[0]["epoch"].as_u64() < rows[1]["epoch"].as_u64());
        assert!(rows[1]["epoch"].as_u64() < rows[2]["epoch"].as_u64());
        assert!(rows[2]["identity"].get("start").is_none());
        assert_eq!(rows[2]["observation_proven"], false);
    }

    #[test]
    fn fd_entries_retain_returned_number_and_type_without_old_identity() {
        let transcript = window(|| {
            process_context(42, 123, Some(501));
            fd_entry("list", 42, 7, 1);
            fd_list("list", 42, Outcome::listed(1, 1, false));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["reads"][0];
        assert_eq!(row["identity"]["fd"], 7);
        assert!(row["identity"].get("start").is_none());
        assert_eq!(row["outcome"]["answer"]["answer"], "1");
        assert_eq!(row["observation_proven"], false);
    }

    #[test]
    fn child_stat_has_one_bound_row_for_present_absent_and_refusal() {
        let transcript = window(|| {
            child_stat("stat", (1, 2), "present", Some((1, 3, "file")), None);
            child_stat("stat", (1, 2), "absent", None, None);
            child_stat("stat", (1, 2), "unreadable", None, Some("EACCES"));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let rows = document["reads"].as_array().expect("reads");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["identity"]["inode"], 3);
        assert_eq!(rows[0]["observation_proven"], true);
        assert_eq!(rows[1]["outcome"], "absent");
        assert_eq!(rows[1]["observation_proven"], true);
        assert_eq!(rows[2]["observation_proven"], false);
    }

    #[test]
    fn descriptor_bound_read_records_parent_and_own_facts() {
        let transcript = window(|| {
            read_present_component_bound("read", (1, 2), "entry", (1, 3), 7, b"data", Some(16));
            read_present_component("read", (1, 2), "entry", b"data", Some(16));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["reads"][0]["identity"]["parent_inode"], 2);
        assert_eq!(document["reads"][0]["identity"]["fd"], 7);
        assert_eq!(document["reads"][0]["identity"]["inode"], 3);
        assert_eq!(document["reads"][0]["observation_proven"], true);
        assert_eq!(document["reads"][1]["observation_proven"], false);
    }

    #[test]
    fn non_utf8_component_is_refused_without_a_lossy_identity() {
        use std::os::unix::ffi::OsStrExt;
        let transcript = window(|| {
            dir_entry(
                "listing",
                Some((1, 2)),
                std::ffi::OsStr::from_bytes(b"x\xff"),
            )
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["reads"][0];
        assert!(row["identity"].get("component").is_none());
        assert_eq!(row["outcome"]["refused"]["class"], "non_utf8_component");
        assert_eq!(row["observation_proven"], false);
    }

    #[test]
    fn vnode_buffer_facts_do_not_manufacture_process_binding() {
        let transcript = window(|| vnode_observed("vnode", 42, 3, 12, 34));
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["reads"][0];
        assert_eq!(row["identity"]["device"], 12);
        assert_eq!(row["identity"]["inode"], 34);
        assert!(row["identity"].get("start").is_none());
        assert_eq!(row["observation_proven"], false);
    }

    #[test]
    fn platform_row_preserves_observed_process_identity() {
        let transcript = window(|| platform("platform", 42, 100, Some(501), "platform_binary"));
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["reads"][0];
        assert_eq!(row["identity"]["pid"], 42);
        assert_eq!(row["identity"]["start"], 100);
        assert_eq!(row["identity"]["uid"], 501);
        assert_eq!(row["observation_proven"], true);
    }

    #[test]
    fn unverified_platform_is_never_positive_coverage() {
        let transcript = window(|| platform("platform", 42, 100, Some(501), "not_verified:gone"));
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["reads"][0];
        assert_eq!(row["outcome"]["refused"]["class"], "not_verified:gone");
        assert_eq!(row["observation_proven"], false);
    }

    #[test]
    fn child_effect_preserves_parent_and_both_rename_components() {
        let transcript = window(|| {
            child_effect("rename", (12, 34), "old", Some("new"));
            effect_outcome("rename", Path::new("old"), None);
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["effects"][0];
        assert_eq!(row["parent_device"], 12);
        assert_eq!(row["parent_inode"], 34);
        assert_eq!(row["path"], "old");
        assert_eq!(row["destination"], "new");
        assert_eq!(row["outcome"], "succeeded");
    }

    #[test]
    fn successful_rename_remains_an_effect_when_later_sync_fails() {
        let transcript = window(|| {
            io_effect("create", Path::new("tmp"), || Ok(())).expect("create");
            io_rename(Path::new("tmp"), Path::new("target"), || Ok(())).expect("rename");
            let error = std::io::Error::from_raw_os_error(libc::EIO);
            sync_observation("directory_sync", (1, 2), None, Some(&error));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let effects = document["effects"].as_array().expect("effects");
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[1]["kind"], "rename");
        assert_eq!(effects[1]["destination"], "target");
        assert_eq!(effects[1]["outcome"], "succeeded");
        assert_eq!(document["reads"][0]["outcome"]["refused"]["class"], "EIO");
    }

    #[test]
    fn atomic_publication_records_file_sync_before_rename_and_directory_sync() {
        let fixture = crate::testkit::TempDir::new("tape-state-publication");
        let target = fixture.path().join("record.json");
        let transcript = window(|| {
            crate::state::publish_atomic(&target, b"record").expect("publication succeeds");
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let effects = document["effects"].as_array().expect("effects");
        let reads = document["reads"].as_array().expect("reads");
        let file_sync = reads
            .iter()
            .find(|row| row["site"] == "state::write_and_publish")
            .expect("file sync was recorded");
        let directory_sync = reads
            .iter()
            .find(|row| row["site"] == "state::sync_directory")
            .expect("directory sync was recorded");
        let rename = effects
            .iter()
            .find(|row| row["kind"] == "rename")
            .expect("rename was recorded");
        assert_eq!(file_sync["outcome"]["answer"]["answer"], "succeeded");
        assert_eq!(directory_sync["outcome"]["answer"]["answer"], "succeeded");
        assert_eq!(rename["outcome"], "succeeded");
        let file_epoch = file_sync["epoch"].as_u64().expect("file sync epoch");
        let rename_epoch = rename["epoch"].as_u64().expect("rename epoch");
        let directory_epoch = directory_sync["epoch"]
            .as_u64()
            .expect("directory sync epoch");
        assert!(file_epoch < rename_epoch);
        assert!(rename_epoch < directory_epoch);
        assert_eq!(file_sync["observation_proven"], false);
    }

    #[test]
    fn failed_save_does_not_claim_that_no_bytes_were_written() {
        let transcript = window(|| {
            let _ = io_effect("save", Path::new("file"), || -> std::io::Result<()> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            });
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        let row = &document["effects"][0];
        assert_eq!(row["kind"], "save");
        assert_eq!(row["outcome"], "EIO");
        assert_eq!(row["partial_effect_possible"], true);
        assert!(row["site"]
            .as_str()
            .is_some_and(|site| site.contains("tape.rs:")));
        let other = window(|| {
            let _ = io_effect("create", Path::new("file"), || -> std::io::Result<()> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            });
        });
        let document: serde_json::Value = serde_json::from_str(&other).expect("transcript");
        assert_eq!(document["effects"][0]["partial_effect_possible"], true);
    }

    #[test]
    fn path_sync_records_success_and_failure_without_inventing_identity() {
        let transcript = window(|| {
            sync_path("state::sync_directory", Path::new("parent"), None);
            let error = std::io::Error::from_raw_os_error(libc::EIO);
            change_errno_to_enoent();
            sync_path("state::sync_directory", Path::new("parent"), Some(&error));
            sync_path(
                "state::write_and_publish",
                Path::new("temporary"),
                Some(&error),
            );
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(
            document["reads"][0]["outcome"]["answer"]["answer"],
            "succeeded"
        );
        assert_eq!(document["reads"][1]["outcome"]["refused"]["class"], "EIO");
        assert_eq!(document["reads"][2]["site"], "state::write_and_publish");
        assert_eq!(document["reads"][2]["outcome"]["refused"]["class"], "EIO");
        assert_eq!(document["reads"][0]["observation_proven"], false);
    }

    #[test]
    fn errno_classes_follow_the_build_platform_constants() {
        for (code, expected) in [
            (libc::ELOOP, "ELOOP"),
            (libc::ENAMETOOLONG, "ENAMETOOLONG"),
            (libc::ENOTDIR, "ENOTDIR"),
            (libc::EACCES, "EACCES"),
        ] {
            assert_eq!(
                errno_class(&std::io::Error::from_raw_os_error(code)),
                expected
            );
        }
    }

    #[test]
    fn effect_outcome_uses_captured_error_after_errno_changes() {
        let transcript = window(|| {
            write_effect("create", Path::new("child"));
            let captured = std::io::Error::from_raw_os_error(libc::EACCES);
            change_errno_to_enoent();
            effect_outcome("create", Path::new("child"), Some(&captured));
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["effects"].as_array().map(Vec::len), Some(1));
        assert_eq!(document["effects"][0]["outcome"], "EACCES");
    }

    #[test]
    fn non_utf8_effect_results_bind_to_the_exact_attempt() {
        use std::os::unix::ffi::OsStrExt;

        let first = Path::new(std::ffi::OsStr::from_bytes(b"first\xff"));
        let second = Path::new(std::ffi::OsStr::from_bytes(b"second\xff"));
        let transcript = window(|| {
            write_effect("create", first);
            write_effect("create", second);
            let error = std::io::Error::from_raw_os_error(libc::EACCES);
            effect_outcome("create", first, Some(&error));
            effect_outcome("create", second, None);
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert!(document["effects"][0]["path"].is_null());
        assert!(document["effects"][1]["path"].is_null());
        assert_eq!(document["effects"][0]["outcome"], "EACCES");
        assert_eq!(document["effects"][1]["outcome"], "succeeded");
    }

    #[test]
    fn clock_row_is_the_value_returned_to_the_caller() {
        let mut returned = 0;
        let expected_line = line!() + 1;
        let transcript = window(|| returned = crate::state::now_ms());
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["clocks"].as_array().map(Vec::len), Some(1));
        assert_eq!(document["clocks"][0]["value_ms"], returned);
        let site = document["clocks"][0]["site"].as_str().expect("caller site");
        assert!(
            site.ends_with(&format!("tape.rs:{expected_line}")),
            "{site}"
        );
    }

    #[test]
    fn process_cap_includes_retry_identity_observations() {
        let transcript = window(|| {
            for pid in 1..CAP_PROCESS_ROWS as u32 {
                process_row("table", pid, Some(501), u64::from(pid));
            }
            observed_process("retry", 1, 1000, Some(501), "classified");
            observed_process("retry", 1, 1001, Some(501), "retry_identity");
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["caps"]["process_rows"], CAP_PROCESS_ROWS);
        assert_eq!(document["caps"]["dropped_rows"], 1);
        assert_eq!(document["caps"]["process_rows_exhausted"], true);
    }

    #[test]
    fn process_cap_counts_every_dropped_row_without_consuming_epochs() {
        let transcript = window(|| {
            for pid in 1..=(CAP_PROCESS_ROWS as u32 + 2) {
                process_row("test", pid, Some(501), u64::from(pid));
            }
            now_ms(7);
        });
        let document: serde_json::Value = serde_json::from_str(&transcript).expect("transcript");
        assert_eq!(document["caps"]["process_rows"], CAP_PROCESS_ROWS);
        assert_eq!(document["caps"]["dropped_rows"], 2);
        assert_eq!(document["caps"]["process_rows_exhausted"], true);
        assert_eq!(document["clocks"][0]["epoch"], CAP_PROCESS_ROWS + 1);
    }

    /// The statement discipline, as far as text can check it: every
    /// instrumented production file has a cfg(test) guard. This count does not
    /// establish per-call reachability or a complete source-site inventory. The mechanical proof is the
    /// release-profile compile (`effigy rust:compile:release`): the tape module
    /// does not exist outside `cfg(test)`, so a call site that leaked would
    /// fail to compile in every production build.
    #[test]
    fn every_tape_call_site_is_cfg_test_only() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        for name in [
            "os/macos.rs",
            "os/absence.rs",
            "state.rs",
            "registry.rs",
            "reference.rs",
            "pool_custody.rs",
            "identity.rs",
            "area.rs",
            "custody.rs",
        ] {
            let source = std::fs::read_to_string(manifest.join(name))
                .unwrap_or_else(|error| panic!("cannot read {name}: {error}"));
            let guards = source.matches("#[cfg(test)]").count();
            let calls = source.matches("crate::tape::").count();
            assert!(
                calls == 0 || guards >= 1,
                "{name}: {calls} tape calls but no #[cfg(test)] attributes"
            );
            checked += calls;
        }
        assert!(
            checked >= 40,
            "expected the full leaf inventory, saw {checked}"
        );
    }

    /// The emitted-shape regression: every row kind the recorder writes is
    /// emitted through the real leaves, parsed as `/2`, and the reviewed field
    /// rules hold. This is emitter conformance, not coverage or parity.
    #[test]
    fn emitted_transcript_has_the_reviewed_two_shape() {
        let fixture = crate::testkit::TempDir::under_scratch("tape-emitted-shape");
        std::fs::write(fixture.path().join("a.json"), b"one").expect("fixture file");
        begin_window(WindowConfig {
            case: "EMITTED-SHAPE".to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "embedded-unverified".to_owned(),
        })
        .expect("the window opens");
        begin_fixture(fixture.path());
        read_absent("absent-site", Path::new("/fixture/missing"));
        read_present(
            "digest-site",
            Path::new("/fixture/a.json"),
            Some((1, 2)),
            b"one",
            None,
        );
        read_present(
            "capped-site",
            Path::new("/fixture/a.json"),
            Some((1, 2)),
            b"one",
            Some(0),
        );
        read_present(
            "bounded-site",
            Path::new("/fixture/a.json"),
            Some((1, 2)),
            b"one",
            Some(64),
        );
        child_stat("child-site", (1, 2), "a.json", Some((1, 3, "file")), None);
        handle_observation("handle-site", "stat", 7, 5, 6, Outcome::facts("directory"));
        linked_handle_observation(
            "MacOs::dir_facts",
            "stat",
            7,
            (5, 6),
            (5, 6),
            Outcome::facts("directory"),
        );
        volume(
            "volume-site",
            Identity::volume_at(
                Path::new("/mount"),
                9,
                Some("volume"),
                Some(Path::new("/mount")),
            ),
            Outcome::facts("mounted"),
        );
        recorded_holder("lock-holder", 9, 42, 77, None, None);
        now_ms(1234);
        write_effect("save", Path::new("/fixture/attempt-ledger"));
        write_effect("save", Path::new("/fixture/succeeded-ledger"));
        effect_outcome("save", Path::new("/fixture/succeeded-ledger"), None);
        let partial = std::io::Error::from_raw_os_error(libc::EIO);
        write_effect("save", Path::new("/fixture/partial-ledger"));
        effect_outcome("save", Path::new("/fixture/partial-ledger"), Some(&partial));
        write_effect("create", Path::new("/fixture/failed-ledger"));
        effect_outcome(
            "create",
            Path::new("/fixture/failed-ledger"),
            Some(&std::io::Error::from_raw_os_error(libc::EACCES)),
        );
        write_effect("save", Path::new("/fixture/dup-ledger"));
        effect_outcome("save", Path::new("/fixture/dup-ledger"), None);
        write_effect("save", Path::new("/fixture/dup-ledger"));
        effect_outcome("save", Path::new("/fixture/dup-ledger"), None);
        std::fs::write(fixture.path().join("a.json"), b"two").expect("fixture file change");
        std::fs::write(fixture.path().join("new.json"), b"new").expect("fixture file added");
        let recorded = finish_completed();
        let wire = shared_transcript(&recorded).expect("the wire converts");
        let parsed = capture_wire::parse_transcript(wire.as_bytes()).expect("the wire parses");
        let at = |pointer: &str| {
            parsed
                .pointer(pointer)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
        assert_eq!(at("/schema"), serde_json::json!(TRANSCRIPT_SCHEMA));
        assert_eq!(at("/version"), serde_json::json!(TRANSCRIPT_SCHEMA));
        assert!(at("/manifest_sha256").is_string());
        assert!(at("/window_monotonic_start_ns").is_null());
        assert!(at("/window_monotonic_end_ns").is_null());
        assert!(at("/reads/0/kind") == at("/reads/0/site"));
        // Null is no cap; zero is a real zero cap; 64 is the real bound.
        assert!(at("/reads/1/cap").is_null(), "{}", at("/reads/1/cap"));
        assert_eq!(at("/reads/2/cap"), serde_json::json!(0));
        assert_eq!(at("/reads/3/cap"), serde_json::json!(64));
        // A handle-child row keeps its handle parent; a descriptor row keeps
        // its own handle identity and its context link.
        assert_eq!(at("/reads/4/identity/parent_source"), "handle");
        assert_eq!(at("/reads/5/identity/fd"), serde_json::json!(7));
        assert_eq!(at("/reads/5/identity/own_source"), "handle");
        assert_eq!(at("/reads/6/identity/parent_source"), "context");
        // A volume row keeps its UUID. The observed numeric device has no inode
        // beside it, and the wire's device/inode pair is both-or-neither, so it
        // stays in the recorder document and the wire states no own identity.
        assert_eq!(at("/reads/7/identity/volume_uuid"), "volume");
        assert!(at("/reads/7/identity/device").is_null());
        assert_eq!(at("/reads/7/identity/own_source"), "none");
        let recorder: serde_json::Value = serde_json::from_str(&recorded).expect("JSON");
        assert_eq!(
            recorder["reads"][7]["identity"]["device"],
            serde_json::json!(9)
        );
        // A recorded holder keeps its unproven kernel start and uid as null.
        assert_eq!(at("/recorded_holders/0/pid"), serde_json::json!(42));
        assert!(at("/recorded_holders/0/kernel_start").is_null());
        assert!(at("/recorded_holders/0/uid").is_null());
        assert_eq!(at("/clocks/0/value"), serde_json::json!(1234));
        // An attempt is unknown; a completed row is succeeded; an error keeps
        // its partial result (this recorder cannot prove none was written, so
        // it never claims `failed`); a repeated completed row stays repeated.
        assert!(at("/effects/0/attempted") == serde_json::json!(true));
        assert_eq!(at("/effects/0/result"), "unknown");
        assert_eq!(at("/effects/1/result"), "succeeded");
        assert_eq!(at("/effects/2/result"), "partial");
        assert_eq!(at("/effects/3/result"), "partial");
        assert_eq!(at("/effects/4/result"), "succeeded");
        assert_eq!(at("/effects/5/result"), "succeeded");
        // Fixture entries use the wire identity names, and the net change is an
        // observed unknown-result diff, never a completed effect.
        let entries = at("/fixture_capture/post");
        let first = entries
            .as_array()
            .and_then(|rows| rows.first())
            .cloned()
            .expect("a fixture entry");
        assert!(first["device"].is_u64());
        assert!(first.get("dev").is_none());
        assert_eq!(at("/fixture_capture/diff_effects/0/result"), "unknown");
        assert_eq!(
            at("/fixture_capture/diff_effects/0/epoch"),
            serde_json::json!(0)
        );
        assert_eq!(at("/outcome/state"), "completed");
        assert_eq!(at("/outcome/result/preview"), serde_json::json!(true));
        // The same bytes tampered, as `/1`, or as the apparatus diagnostic are
        // all refused by the transcript parser.
        let mut tampered: serde_json::Value = parsed.clone();
        tampered["reads"][1]["cap"] = serde_json::json!("none");
        assert!(capture_wire::parse_transcript(tampered.to_string().as_bytes()).is_err());
        let mut historical = parsed.clone();
        historical["schema"] = serde_json::json!("oxide.capture.transcript/1");
        historical["version"] = serde_json::json!("oxide.capture.transcript/1");
        let refusal = capture_wire::parse_transcript(historical.to_string().as_bytes())
            .expect_err("a /1 document is refused");
        assert!(refusal.message.contains("historical /1"), "{refusal:?}");
        let apparatus =
            apparatus_document("EMITTED-SHAPE", Some(&"0".repeat(64)), "manifest refused");
        assert!(capture_wire::parse_transcript(apparatus.as_bytes()).is_err());
    }

    /// The shared wire's identity key is narrower than the recorder's: it has
    /// no fd, pid, start, uid or volume UUID. Many real observations therefore
    /// share one wire key, and the shared 64-occurrence cap truncates them. The
    /// truncation is recorded, the rows stay in the recorder document, and the
    /// window is insufficient by rule.
    #[test]
    fn the_wire_occurrence_cap_truncates_a_collapsed_key() {
        begin_window(WindowConfig {
            case: "COLLAPSED-KEY".to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        for pid in 1..=CAP_OCCURRENCES + 6 {
            process_row("process-table-site", pid, Some(501), u64::from(pid));
        }
        let recorded = finish_completed();
        let wire = shared_transcript(&recorded).expect("the wire converts");
        let parsed = capture_wire::parse_transcript(wire.as_bytes()).expect("the wire parses");
        let reads = parsed["reads"].as_array().expect("read rows");
        assert_eq!(reads.len(), CAP_OCCURRENCES as usize);
        assert_eq!(
            parsed["truncation"]["occurrences_truncated"],
            serde_json::json!(true)
        );
        assert!(
            reads
                .iter()
                .all(|row| row["kind"] == "process-table-site#process-row"),
            "the wire site names the source symbol and the observation family"
        );
        let recorder: serde_json::Value = serde_json::from_str(&recorded).expect("JSON");
        assert_eq!(
            recorder["reads"].as_array().map(Vec::len),
            Some(CAP_OCCURRENCES as usize + 6),
            "every recorded row stays in the recorder document"
        );
    }

    #[test]
    fn an_unrepresentable_effect_path_is_a_descriptor_effect() {
        use std::os::unix::ffi::OsStrExt;
        begin_window(WindowConfig {
            case: "UNREPRESENTABLE".to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        let path = Path::new(std::ffi::OsStr::from_bytes(b"name\xff"));
        write_effect("create", path);
        effect_outcome(
            "create",
            path,
            Some(&std::io::Error::from_raw_os_error(libc::EACCES)),
        );
        let recorded = finish_completed();
        let wire = shared_transcript(&recorded).expect("the wire converts");
        let parsed = capture_wire::parse_transcript(wire.as_bytes()).expect("the wire parses");
        assert_eq!(parsed["effects"][0]["kind"], "descriptor-effect");
        assert!(parsed["effects"][0]["path"].is_null());
        assert_eq!(parsed["effects"][0]["result"], "partial");
        assert_eq!(parsed["effects"][0]["attempted"], serde_json::json!(false));
        // The recorder document keeps the real effect kind and its path state.
        let recorder: serde_json::Value = serde_json::from_str(&recorded).expect("JSON");
        assert_eq!(recorder["effects"][0]["kind"], "create");
        assert!(recorder["effects"][0]["path"].is_null());
    }

    #[test]
    fn fixture_capture_reports_the_net_change_it_observed() {
        let fixture = crate::testkit::TempDir::under_scratch("tape-fixture-capture");
        std::fs::write(fixture.path().join("kept.json"), b"kept").expect("fixture file");
        std::fs::write(fixture.path().join("changed.json"), b"before").expect("fixture file");
        begin_window(WindowConfig {
            case: "FIXTURE-CAPTURE".to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        begin_fixture(fixture.path());
        std::fs::write(fixture.path().join("changed.json"), b"after").expect("change");
        std::fs::write(fixture.path().join("added.json"), b"added").expect("add");
        std::fs::remove_file(fixture.path().join("kept.json")).expect("remove");
        let recorded = finish_completed();
        let wire = shared_transcript(&recorded).expect("the wire converts");
        let parsed = capture_wire::parse_transcript(wire.as_bytes()).expect("the wire parses");
        let diff = parsed["fixture_capture"]["diff_effects"]
            .as_array()
            .expect("diff rows");
        let kinds: Vec<(&str, &str)> = diff
            .iter()
            .map(|row| {
                (
                    row["path"].as_str().unwrap_or(""),
                    row["kind"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert!(kinds.contains(&("added.json", "create")), "{kinds:?}");
        assert!(kinds.contains(&("changed.json", "save")), "{kinds:?}");
        assert!(kinds.contains(&("kept.json", "unlink")), "{kinds:?}");
        assert!(diff.iter().all(|row| row["result"] == "unknown"));
        assert_eq!(
            parsed["fixture_capture"]["truncated"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn a_direct_read_site_cannot_launder_a_handle_parent() {
        // Real rows at direct-read sites carry no handle provenance.
        let recorded = window(|| {
            read_absent("state::read_bounded_inner", Path::new("/fixture/registry"));
            stat("MacOs::path_facts", Path::new("/fixture/area"), None);
            extracted_dir_entry(
                "area::read_directory",
                Some((1, 2)),
                std::ffi::OsStr::new("entry"),
            );
        });
        let document: serde_json::Value = serde_json::from_str(&recorded).expect("JSON");
        let rows = document["reads"].as_array().expect("rows");
        assert!(rows.iter().any(|row| {
            row["site"] == "area::read_directory" && row["identity"]["parent_source"] == "extracted"
        }));
        let mut identity = Identity::component(Some((1, 2)), "entry");
        identity.parent_source = Provenance::Handle;
        let doctored = vec![ReadRow {
            kind: "dir-entry",
            site: "area::read_directory",
            identity,
            outcome: Outcome::facts("entry"),
            observation_proven: true,
            cap: None,
            epoch: 1,
            occurrence: 1,
        }];
        let issues = laundered_parent_rows(&doctored);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("area::read_directory"));
    }

    #[test]
    fn a_handle_reread_must_agree_with_its_context_identity() {
        let recorded = window(|| {
            linked_handle_observation(
                "MacOs::recheck_dir",
                "stat",
                7,
                (5, 6),
                (5, 6),
                Outcome::facts("directory"),
            );
        });
        let document: serde_json::Value = serde_json::from_str(&recorded).expect("JSON");
        let row = &document["reads"][0];
        assert_eq!(row["identity"]["parent_source"], "context");
        assert_eq!(row["identity"]["own_source"], "handle");
        let coherent = vec![ReadRow {
            kind: "stat",
            site: "MacOs::recheck_dir",
            identity: Identity {
                device: Some(5),
                inode: Some(6),
                own_source: Provenance::Handle,
                parent_device: Some(5),
                parent_inode: Some(6),
                parent_source: Provenance::Context,
                ..Identity::none()
            },
            outcome: Outcome::facts("directory"),
            observation_proven: false,
            cap: None,
            epoch: 1,
            occurrence: 1,
        }];
        assert!(incoherent_handle_rereads(&coherent).is_empty());
        let mut incoherent = coherent;
        incoherent[0].identity.inode = Some(7);
        let issues = incoherent_handle_rereads(&incoherent);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("MacOs::recheck_dir"));
    }

    /// The declaration shape: the tape is `pub(crate)` under `cfg(test)` in
    /// `lib.rs`, so no public surface grows and Oxide's dependency cannot
    /// reach it.
    #[test]
    fn the_tape_is_a_private_cfg_test_module() {
        let lib = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
            .unwrap_or_else(|error| panic!("cannot read lib.rs: {error}"));
        let declaration = lib
            .lines()
            .find(|line| line.contains("mod tape;"))
            .expect("lib.rs declares the tape module");
        assert!(declaration.contains("pub(crate)"), "{declaration}");
        let position = lib
            .lines()
            .position(|line| line.contains("mod tape;"))
            .expect("the declaration has a position");
        let before = lib
            .lines()
            .take(position)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            before.contains("#[cfg(test)]"),
            "the tape module is declared under #[cfg(test)]: {before}"
        );
    }
}
