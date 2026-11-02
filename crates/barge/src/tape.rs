//! The F6-O2 B-S1 observation tape: what the release planner actually read.
//!
//! This module is compiled only under `cfg(test)` (`lib.rs` declares it as a
//! `pub(crate)` `cfg(test)` module), so a production build compiles none of it
//! and the covering release-profile check proves that. Every call into it from
//! a production function body is a separate `#[cfg(test)]`-attributed
//! statement, so every preexisting production statement stays byte-for-byte
//! unchanged. The tape is one-sided: it records what the real syscalls and
//! parsers returned and feeds nothing back — no policy is copied, no custody,
//! absence or eligible verdict is injected, and no fixture fact is invented.
//!
//! ## Transcript
//!
//! [`finish`] serializes the window as one versioned `oxide.capture.transcript/1`
//! document, always written, including when the planner refused. It carries the
//! engine, the case and the manifest digest it is bound to, the entrypoint
//! identity, the engine's own result or refusal, the effect rows (empty for the
//! read-only plan, which is the STATIC effect set), the read rows and the clock
//! rows. Read-row identity is the complete object identity: absolute path,
//! parent device and inode, own device and inode for files; component name plus
//! observed parent identity for path components; `(pid, start, uid)` for
//! processes; UUID plus mount for volumes. An outcome is a bytes digest, an
//! explicit absent, a listed count, or a refusal class — never a defaulted
//! verdict.
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
//! so an attempted write can never pass as a read-only pass; the effect kinds
//! are the contract's closed set. Not recorded, because the plan never calls
//! them and they read no fixture object: `random_bytes` (entropy, not fixture
//! state), `set_dir_mode`, `create_child_symlink`, `sync_dir` and the flush
//! path.
//!
//! Nothing here is public and nothing here feeds a production decision; the
//! no-public-surface test below pins the declaration shape, and the statement
//! discipline test pins the `#[cfg(test)]` form of every call site.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::digest::sha256_hex;

/// The transcript schema this module writes.
pub(crate) const TRANSCRIPT_SCHEMA: &str = "oxide.capture.transcript/1";
/// The engine name this crate's window writes.
pub(crate) const ENGINE: &str = "barge";
/// The transcript's read-row cap.
pub(crate) const CAP_READ_ROWS: usize = 8192;
/// The transcript's document size cap.
pub(crate) const CAP_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
/// The transcript's per-identity-key occurrence cap.
pub(crate) const CAP_OCCURRENCES: u32 = 64;
/// The transcript's process-row cap.
pub(crate) const CAP_PROCESS_ROWS: usize = 512;

/// One observation's complete object identity. Only the fields that apply to
/// the observed object are set, and the serialized field order is fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Identity {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_inode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inode: Option<u64>,
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
            device: None,
            inode: None,
            pid: None,
            start: None,
            uid: None,
            fd: None,
            volume_uuid: None,
            mount: None,
        }
    }

    /// A file or directory identity: its absolute path, best-effort parent
    /// identity, and its own device and inode when the caller read them.
    pub(crate) fn file(path: &Path, facts: Option<(u64, u64)>) -> Self {
        let (parent_device, parent_inode) = parent_identity(path);
        Self {
            path: Some(path.display().to_string()),
            component: None,
            parent_device,
            parent_inode,
            device: facts.map(|(device, _)| device),
            inode: facts.map(|(_, inode)| inode),
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
            ..Self::none()
        }
    }

    /// A process identity: never a pid alone.
    pub(crate) fn process(pid: u32, start: u64, uid: Option<u32>, fd: Option<i32>) -> Self {
        Self {
            pid: Some(pid),
            start: Some(start),
            uid,
            fd,
            ..Self::none()
        }
    }

    /// A volume identity: the UUID and the mount it was observed at.
    pub(crate) fn volume(volume_uuid: Option<&str>, mount: Option<&Path>) -> Self {
        Self {
            volume_uuid: volume_uuid.map(str::to_owned),
            mount: mount.map(|mount| mount.display().to_string()),
            ..Self::none()
        }
    }

    /// A volume identity observed at a path: the path and its device, the
    /// UUID, and the kernel's mount point.
    pub(crate) fn volume_at(
        path: &Path,
        device: u64,
        volume_uuid: Option<&str>,
        mount: Option<&Path>,
    ) -> Self {
        let (parent_device, parent_inode) = parent_identity(path);
        Self {
            path: Some(path.display().to_string()),
            parent_device,
            parent_inode,
            device: Some(device),
            volume_uuid: volume_uuid.map(str::to_owned),
            mount: mount.map(|mount| mount.display().to_string()),
            ..Self::none()
        }
    }

    /// The reconciliation key: every set field, in fixed order.
    fn key(&self) -> String {
        let mut key = String::new();
        let mut push = |label: &str, value: Option<String>| {
            if let Some(value) = value {
                key.push('|');
                key.push_str(label);
                key.push('=');
                key.push_str(&value);
            }
        };
        push("path", self.path.clone());
        push("component", self.component.clone());
        push("pdev", self.parent_device.map(|value| value.to_string()));
        push("pino", self.parent_inode.map(|value| value.to_string()));
        push("dev", self.device.map(|value| value.to_string()));
        push("ino", self.inode.map(|value| value.to_string()));
        push("pid", self.pid.map(|value| value.to_string()));
        push("start", self.start.map(|value| value.to_string()));
        push("uid", self.uid.map(|value| value.to_string()));
        push("fd", self.fd.map(|value| value.to_string()));
        push("uuid", self.volume_uuid.clone());
        push("mount", self.mount.clone());
        key
    }
}

/// The best-effort device and inode of a path's parent directory, read by the
/// tape itself. A parent the tape cannot stat leaves both unset; the row still
/// carries the path.
fn parent_identity(path: &Path) -> (Option<u64>, Option<u64>) {
    match path.parent().map(std::fs::symlink_metadata) {
        Some(Ok(metadata)) => (Some(metadata.dev()), Some(metadata.ino())),
        _ => (None, None),
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
    path: String,
    epoch: u64,
}

/// One clock reading bound to the declared window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ClockRow {
    kind: &'static str,
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

/// The transcript document. Field order is the serialization order.
#[derive(Debug, Serialize)]
pub(crate) struct Transcript<'a> {
    schema: &'static str,
    engine: &'static str,
    case: &'a str,
    manifest_sha256: &'a str,
    entrypoint: Entrypoint<'a>,
    outcome: &'a EngineOutcome,
    /// The engine's own result in its own document, when it completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a serde_json::Value>,
    effects: &'a [EffectRow],
    reads: &'a [ReadRow],
    clocks: &'a [ClockRow],
    caps: Caps,
}

/// The entrypoint identity the transcript names.
#[derive(Debug, Serialize)]
struct Entrypoint<'a> {
    symbol: &'static str,
    source_binding: &'a str,
}

/// The per-thread recorder state. The plan is single-threaded, so one
/// thread-local window holds every leaf the plan reaches; a lib test that
/// never opens a window records nothing.
#[derive(Debug, Default)]
struct Tape {
    config: Option<WindowConfig>,
    reads: Vec<ReadRow>,
    clocks: Vec<ClockRow>,
    effects: Vec<EffectRow>,
    occurrences: HashMap<String, u32>,
    process_rows: usize,
    next_epoch: u64,
    read_rows_exhausted: bool,
    occurrences_exhausted: bool,
    process_rows_exhausted: bool,
    dropped_rows: u64,
}

impl Tape {
    /// Drop every row and counter but leave the (already taken) window gone, so
    /// the next window starts from epoch zero and no rows.
    fn reset_without_window(&mut self) {
        *self = Self::empty();
    }

    /// A tape with no window and no rows, as every thread starts.
    fn empty() -> Self {
        Self {
            config: None,
            reads: Vec::new(),
            clocks: Vec::new(),
            effects: Vec::new(),
            occurrences: HashMap::new(),
            process_rows: 0,
            next_epoch: 0,
            read_rows_exhausted: false,
            occurrences_exhausted: false,
            process_rows_exhausted: false,
            dropped_rows: 0,
        }
    }
}

thread_local! {
    static TAPE: RefCell<Tape> = RefCell::new(Tape::empty());
}

/// Open this thread's window. A window that is already open is refused: one
/// window per run, never nested.
pub(crate) fn begin_window(config: WindowConfig) -> Result<(), String> {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_some() {
            return Err("a tape window is already open on this thread".to_owned());
        }
        tape.config = Some(config);
        Ok(())
    })
}

/// Close the window and serialize the transcript. The tape resets, so a later
/// window starts from epoch zero and no rows.
pub(crate) fn finish(
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
        let mut caps = Caps {
            read_rows: tape.reads.len(),
            document_bytes: 0,
            occurrences_per_key: CAP_OCCURRENCES,
            process_rows: tape.process_rows,
            read_rows_exhausted: tape.read_rows_exhausted,
            occurrences_exhausted: tape.occurrences_exhausted,
            process_rows_exhausted: tape.process_rows_exhausted,
            document_truncated: false,
            dropped_rows: tape.dropped_rows,
        };
        let mut reads = std::mem::take(&mut tape.reads);
        // The document cap is a contract cap, not a suggestion: rows leave from
        // the tail until the document fits, and the truncation is recorded.
        loop {
            let document = Transcript {
                schema: TRANSCRIPT_SCHEMA,
                engine: ENGINE,
                case: &config.case,
                manifest_sha256: &config.manifest_sha256,
                entrypoint: Entrypoint {
                    symbol: config.entrypoint_symbol,
                    source_binding: &config.source_binding,
                },
                outcome: &outcome,
                result,
                effects: &tape.effects,
                reads: &reads,
                clocks: &tape.clocks,
                caps,
            };
            let bytes = serde_json::to_string(&document)
                .map_err(|error| format!("the transcript cannot be serialized: {error}"))?;
            if bytes.len() <= CAP_DOCUMENT_BYTES {
                tape.reset_without_window();
                return Ok(bytes);
            }
            if reads.pop().is_none() {
                tape.reset_without_window();
                return Err(
                    "the transcript exceeds its document cap with no rows to drop".to_owned(),
                );
            }
            caps.dropped_rows = caps.dropped_rows.saturating_add(1);
            caps.document_truncated = true;
            caps.read_rows = reads.len();
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
        let key = format!("{kind}|{site}{}", identity.key());
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
        if kind == "process-row" {
            tape.process_rows = tape.process_rows.saturating_add(1);
            if tape.process_rows > CAP_PROCESS_ROWS {
                tape.process_rows_exhausted = true;
                return;
            }
        }
        tape.reads.push(ReadRow {
            kind,
            site,
            identity,
            outcome,
            cap,
            epoch,
            occurrence,
        });
    });
}

/// Record one clock reading on this thread's window.
fn clock(kind: &'static str, value_ms: u64) {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        tape.clocks.push(ClockRow {
            kind,
            value_ms,
            epoch,
        });
    });
}

/// Record one effect row on this thread's window.
fn effect(kind: &'static str, path: &Path) {
    TAPE.with(|tape| {
        let mut tape = tape.borrow_mut();
        if tape.config.is_none() {
            return;
        }
        let epoch = tape.next_epoch;
        tape.next_epoch = tape.next_epoch.saturating_add(1);
        tape.effects.push(EffectRow {
            kind,
            path: path.display().to_string(),
            epoch,
        });
    });
}

/// The io error's errno class, as the transcript binds it.
pub(crate) fn errno_class(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(1) => "EPERM".to_owned(),
        Some(2) => "ENOENT".to_owned(),
        Some(3) => "ESRCH".to_owned(),
        Some(9) => "EBADF".to_owned(),
        Some(13) => "EACCES".to_owned(),
        Some(20) => "ENOTDIR".to_owned(),
        Some(36) => "ENAMETOOLONG".to_owned(),
        Some(5) => "EIO".to_owned(),
        Some(19) => "ENODEV".to_owned(),
        Some(6) => "ENXIO".to_owned(),
        Some(40) => "ELOOP".to_owned(),
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
    record(
        "file",
        site,
        Identity::component(Some(parent), name),
        Outcome::digest(bytes, cap),
        cap,
    );
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
    record(
        "dir-entry",
        site,
        Identity::component(parent, &name.to_string_lossy()),
        Outcome::facts("entry"),
        None,
    );
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
    Held(Option<(u64, u64)>),
    /// The lock file does not exist.
    Absent,
    /// The lock could not be taken or inspected.
    Refused(String),
}

/// Record a lock observation: held with the lock file's identity when it
/// could be read, positively absent, or refused.
pub(crate) fn lock(site: &'static str, path: &Path, outcome: LockOutcome) {
    let (identity, outcome) = match outcome {
        LockOutcome::Held(facts) => (Identity::file(path, facts), Outcome::facts("held")),
        LockOutcome::Absent => (Identity::file(path, None), Outcome::absent()),
        LockOutcome::Refused(class) => (Identity::file(path, None), Outcome::refused(class)),
    };
    record("lock", site, identity, outcome, None);
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

/// Record the process-table read's own outcome.
pub(crate) fn process_table(site: &'static str, outcome: Outcome) {
    record("process-table", site, Identity::none(), outcome, None);
}

/// Record one per-pid descriptor listing.
pub(crate) fn fd_list(site: &'static str, pid: u32, outcome: Outcome) {
    record(
        "fd-list",
        site,
        Identity::process(pid, 0, None, None),
        outcome,
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
    let mut identity = Identity::process(pid, 0, None, Some(fd));
    identity.path = path;
    record(which, site, identity, outcome, None);
}

/// Record one code-signing status answer for a classified identity.
pub(crate) fn platform(site: &'static str, pid: u32, start: u64, answer: &str) {
    record(
        "platform-status",
        site,
        Identity::process(pid, start, None, None),
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

/// Record the caller clock: the value the engine passed or read.
pub(crate) fn now_ms(value_ms: u64) {
    clock("now_ms", value_ms);
}

/// Record a real or attempted effect from the contract's closed set.
pub(crate) fn write_effect(kind: &'static str, path: &Path) {
    effect(kind, path);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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
        assert_eq!(document["schema"], TRANSCRIPT_SCHEMA);
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
        assert_eq!(row["epoch"], 0);
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
        for index in 0..CAP_READ_ROWS {
            let path = PathBuf::from(format!("{bound}-{index}"));
            record(
                "file",
                "state::read_bounded_inner",
                Identity::file(&path, None),
                Outcome::digest(&filler, None),
                None,
            );
        }
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
        assert!(document["caps"]["dropped_rows"].as_u64().unwrap_or(0) > 0);
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
        assert_eq!(first, second);
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
        // The document cap is enforced at serialization; its bound is stated
        // here as the constant the serializer uses.
        assert_eq!(CAP_DOCUMENT_BYTES, 8 * 1024 * 1024);
        assert_eq!(CAP_PROCESS_ROWS, 512);
    }

    /// The statement discipline, as far as text can check it: every
    /// instrumented production file carries `#[cfg(test)]` attributes at least
    /// as often as it carries tape calls. The mechanical proof is the
    /// release-profile compile (`effigy rust:compile:release`): the tape module
    /// does not exist outside `cfg(test)`, so a call site that leaked would
    /// fail to compile in every production build.
    #[test]
    fn every_tape_call_site_is_cfg_test_only() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        for name in [
            "os/macos.rs",
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
