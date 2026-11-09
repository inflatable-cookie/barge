//! The B-S1 release-plan harness: one manifest window over the actual
//! `release::plan`.
//!
//! Compiled only under `cfg(test)`. The exported executable
//! (`crates/barge/tests/tape_eval.rs`, `harness = false`) and the embedded
//! end-to-end exercise both call [`run`], so the exercised path is the same
//! one an Oxide-side orchestrator invokes. The window is the contract's Barge
//! seam: the harness builds the fixture `StatePaths` through an explicit
//! `override_dir`, supplies a test-only [`AccountFacts`] returning fixture
//! paths, passes the real platform object for all five trait arguments, calls
//! the actual `barge::release::plan` with `ReleaseBounds::design()` and a real
//! `now_ms`, and always writes one `oxide.capture.transcript/2` document —
//! including on refusal. It records only; it feeds nothing back, copies no
//! policy, and never touches the production account resolution or the
//! `proof-synthetic-account` feature. A refusal before the engine ran, or a
//! report that cannot be carried into a conforming transcript, writes the
//! distinct `oxide.capture.apparatus/2` diagnostic instead; that document is
//! never a capture.
//!
//! ## Manifest fields this harness reads
//!
//! The manifest is `oxide.capture.manifest/2`, owned by the orchestrator. The
//! copied Oxide 284 parser validates its reviewed wire shape, separately from
//! completeness; completeness is judged afterwards by the same shared parser
//! and reconciler rules over the emitted transcript. After validation, the
//! harness extracts these members for the engine and binds the whole document's
//! SHA-256:
//!
//! - `case`: the case name carried into the transcript;
//! - `barge.volume_uuid`: the fixture volume's UUID, as registered;
//! - `barge.namespace`, `barge.area`: the planned area;
//! - `barge.named_roots`: optional array of explicitly named roots.
//!
//! A member that is missing or of the wrong shape is an apparatus error: the
//! process exits `2` and writes an `oxide.capture.apparatus/2` diagnostic with
//! no invented engine outcome. The engine's own refusals —
//! an undeclared area, an unregistered volume, an unreadable registry — are
//! engine outcomes: the shared transcript carries `outcome.state` as
//! `refused` with the engine's own refusal reason, and the exit code
//! `ErrorKind::exit_code` gives, and the process exits with that code.
//!
//! ## Retained fixture custody (explicit opt-in, test-only)
//!
//! The ordinary embedded exercise removes its own fixture when it drops. A
//! caller that must census the leader's tree after the child exits opts in by
//! setting `BARGE_TAPE_CUSTODY=barge.tape.kept-fixture/1` together with this
//! invocation's own run root in `BARGE_TEST_SCRATCH_ROOT`; the mode is resolved
//! and every premise is checked before a fixture exists. Under it the fixture
//! leaves automatic cleanup inside the creation call, before any fallible step,
//! its path is announced once as `barge.tape.exercise/1: kept fixture <path>`,
//! and the manifest bytes the fixture already holds are published as
//! `manifest.json` inside it before the window runs. Nothing here removes it
//! afterwards; an ordinary route is unchanged
//! (`docs/knowledge/contracts/o2-tape.md`, "Retained fixture custody").
//!
//! ## Staged provenance for the later X-S2 orchestration
//!
//! The orchestrator stages this exact commit's tree, builds the harness
//! offline from the committed `Cargo.lock` with default features only — no
//! feature is enabled, and the proof feature must stay off — through a
//! locked Effigy build-only selector under the pinned toolchain 1.97.1. The
//! provisional export receipt is checked before a window. The orchestrator invokes the executable with the argv
//! contract `--fixture-root --manifest --transcript-out --case`. The binary
//! receipt digest must be supplied independently; nothing here reads the live pool, root,
//! registry or account.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::area::{AreaDeclaration, PrunePolicy, ReserveParticipation};
use crate::error::{Error, ErrorKind};
use crate::os::AbsenceVolume;
use crate::os::{AccountFacts, DirOps, MountRoot, ProcessObserver, VolumeFacts, VolumeUuid};
use crate::release::{ReleaseBounds, ReleaseRequest};
use crate::state::StatePaths;
use crate::tape::{
    begin_window, finish, finish_apparatus, read_present, WindowConfig, CAP_DOCUMENT_BYTES,
    CAP_OCCURRENCES, CAP_PROCESS_ROWS, CAP_READ_ROWS,
};

/// The manifest members this harness reads, after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestRequest {
    /// The case name from the manifest.
    pub(crate) case: String,
    /// The fixture volume's registered UUID.
    pub(crate) volume_uuid: VolumeUuid,
    /// The area's namespace.
    pub(crate) namespace: String,
    /// The area's name.
    pub(crate) area: String,
    /// The roots an operator named explicitly.
    pub(crate) named_roots: Vec<String>,
}

/// Validate the shared manifest and extract the members the engine consumes.
/// Missing or mistyped required members are refused.
pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<ManifestRequest, String> {
    crate::tape::validate_shared_manifest(bytes)?;
    let document: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("the manifest is not one JSON document: {error}"))?;
    fn member<'a>(
        container: &'a serde_json::Value,
        key: &str,
    ) -> Result<&'a serde_json::Value, String> {
        container
            .get(key)
            .ok_or_else(|| format!("the manifest has no `{key}` member"))
    }
    if member(&document, "schema")?.as_str() != Some(crate::tape::MANIFEST_SCHEMA) {
        return Err(format!(
            "the manifest schema is not {}",
            crate::tape::MANIFEST_SCHEMA
        ));
    }
    let case = member(&document, "case")?
        .as_str()
        .ok_or_else(|| "the manifest's `case` is not a string".to_owned())?
        .to_owned();
    let barge = member(&document, "barge")?;
    let uuid_text = member(barge, "volume_uuid")?
        .as_str()
        .ok_or_else(|| "the manifest's `barge.volume_uuid` is not a string".to_owned())?;
    let namespace = member(barge, "namespace")?
        .as_str()
        .ok_or_else(|| "the manifest's `barge.namespace` is not a string".to_owned())?
        .to_owned();
    let area = member(barge, "area")?
        .as_str()
        .ok_or_else(|| "the manifest's `barge.area` is not a string".to_owned())?
        .to_owned();
    if case.is_empty() || namespace.is_empty() || area.is_empty() {
        return Err("the manifest case, namespace and area must be nonempty".to_owned());
    }
    let named_roots = match member(barge, "named_roots") {
        Ok(value) => value
            .as_array()
            .ok_or_else(|| "the manifest's `barge.named_roots` is not an array".to_owned())?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "a named root is not a string".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Err(_) => Vec::new(),
    };
    let volume_uuid = VolumeUuid::parse(uuid_text)
        .map_err(|error| format!("the manifest's volume UUID is not a UUID: {error}"))?;
    if named_roots.len() > 64 || named_roots.iter().any(String::is_empty) {
        return Err("the manifest named roots are empty or exceed the 64-root bound".to_owned());
    }
    Ok(ManifestRequest {
        case,
        volume_uuid,
        namespace,
        area,
        named_roots,
    })
}

/// The test-only account: every directory this run reads comes from the
/// fixture. It never consults the environment, the password database or the
/// real account home.
#[derive(Debug, Clone)]
pub(crate) struct FixtureAccount {
    /// The fixture root, as the account home.
    pub(crate) home: PathBuf,
    /// The Barge state directory the window plans over.
    pub(crate) state: PathBuf,
    /// Oxide's state directory, inside the fixture.
    pub(crate) tool_state: PathBuf,
    /// Oxide's cache directory, inside the fixture.
    pub(crate) tool_cache: PathBuf,
}

impl AccountFacts for FixtureAccount {
    fn account_home(&self) -> Result<PathBuf, Error> {
        Ok(self.home.clone())
    }

    fn state_root(&self) -> Result<PathBuf, Error> {
        Ok(self.state.clone())
    }

    fn tool_state_dir(&self, tool: &str) -> Result<PathBuf, Error> {
        if tool != crate::inspect::OXIDE_TOOL {
            return Err(Error::new(
                ErrorKind::Usage,
                format!("the fixture account serves Oxide only, not `{tool}`"),
            ));
        }
        Ok(self.tool_state.clone())
    }

    fn tool_cache_dir(&self, tool: &str) -> Result<PathBuf, Error> {
        if tool != crate::inspect::OXIDE_TOOL {
            return Err(Error::new(
                ErrorKind::Usage,
                format!("the fixture account serves Oxide only, not `{tool}`"),
            ));
        }
        Ok(self.tool_cache.clone())
    }
}

/// One window's summary, for the caller's exit code and the embedded
/// exercise's assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunSummary {
    /// `completed` or `refused`, as the transcript records.
    pub(crate) status: &'static str,
    /// The exit code the caller exits with.
    pub(crate) exit_code: u8,
    /// The eligible-row count of the engine's own report, when it completed.
    pub(crate) eligible: Option<usize>,
    /// The transcript rows the window recorded.
    pub(crate) read_rows: usize,
    /// The declared transcript caps, restated for the caller's own check.
    pub(crate) caps: (usize, usize, u32, usize),
    /// The shared completeness issues this window's transcript produces. A
    /// nonempty list is the honest statement that the window is not sufficient
    /// proof; it is never a parity or coverage claim.
    pub(crate) issues: Vec<String>,
}

/// Check the export receipt and its actual source and executable bytes before
/// opening the observation window. The orchestrator independently supplies the
/// expected receipt digest; a receipt beside a substituted binary is not trust.
pub(crate) fn verify_export(
    receipt: &Path,
    expected: &str,
    expected_closure: &str,
) -> Result<String, String> {
    fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let file =
            std::fs::File::open(path).map_err(|e| format!("cannot open binding input: {e}"))?;
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read binding input: {e}"))?;
        if bytes.len() as u64 > limit {
            return Err("binding input exceeds its bound".to_owned());
        }
        Ok(bytes)
    }
    fn hex(value: &str, len: usize) -> bool {
        value.len() == len
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
    fn field<'a>(value: &'a serde_json::Value, key: &str, len: usize) -> Result<&'a str, String> {
        let text = value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("export binding lacks {key}"))?;
        if !hex(text, len) {
            return Err(format!("invalid {key}"));
        }
        Ok(text)
    }
    if !hex(expected, 64) {
        return Err("invalid expected receipt digest".to_owned());
    }
    if !hex(expected_closure, 64) {
        return Err("invalid expected closure digest".to_owned());
    }
    let bytes = read_bounded(receipt, 8 * 1024 * 1024)?;
    if crate::digest::sha256_hex(&bytes) != expected {
        return Err("export receipt digest or size mismatch".to_owned());
    }
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid export receipt: {e}"))?;
    if doc["schema"] != "barge.tape.export/1" {
        return Err("invalid export schema".to_owned());
    }
    let closure_path = receipt
        .parent()
        .ok_or_else(|| "receipt has no parent".to_owned())?
        .join("export-closure.json");
    let closure_bytes = read_bounded(&closure_path, 1024 * 1024)?;
    if crate::digest::sha256_hex(&closure_bytes) != expected_closure {
        return Err("export closure digest mismatch".to_owned());
    }
    let closure: serde_json::Value = serde_json::from_slice(&closure_bytes)
        .map_err(|e| format!("invalid export closure: {e}"))?;
    if closure["schema"] != "barge.tape.export-closure/1"
        || closure["status"] != "passed"
        || closure["scope"] != "live-inventory-comparison-only"
        || closure["receipt_sha256"] != expected
        || closure["live_before_sha256"] != closure["live_after_sha256"]
    {
        return Err("export closure is incomplete or disagrees".to_owned());
    }
    field(&closure, "live_before_sha256", 64)?;
    field(&doc, "commit", 40)?;
    field(&doc, "tree", 40)?;
    let executable = field(&doc, "executable_sha256", 64)?;
    let lock = field(&doc, "lock_sha256", 64)?;
    let toolchain = field(&doc, "toolchain_sha256", 64)?;
    field(&doc, "rustc_sha256", 64)?;
    field(&doc, "oxide_sha256", 64)?;
    let rustc_version = doc["rustc_version"]
        .as_str()
        .ok_or_else(|| "export binding lacks rustc version".to_owned())?;
    if !rustc_version.starts_with("rustc 1.97.1 ") {
        return Err("export used a different Rust toolchain".to_owned());
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let exe_bytes = read_bounded(&exe, 256 * 1024 * 1024)?;
    if crate::digest::sha256_hex(&exe_bytes) != executable {
        return Err("executable digest mismatch".to_owned());
    }
    let root = receipt
        .parent()
        .ok_or_else(|| "receipt has no parent".to_owned())?
        .join("source");
    let root_meta =
        std::fs::symlink_metadata(&root).map_err(|e| format!("cannot inspect source root: {e}"))?;
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return Err("source root is not a directory".to_owned());
    }
    let files = doc["files"]
        .as_object()
        .ok_or_else(|| "missing source files".to_owned())?;
    if files.is_empty() {
        return Err("empty source inventory".to_owned());
    }
    if files
        .get("scripts/tape/private-wrapper.py")
        .and_then(|v| v["sha256"].as_str())
        != Some(field(&closure, "wrapper_sha256", 64)?)
    {
        return Err("export closure names a different wrapper".to_owned());
    }
    let mut expected_files = std::collections::BTreeSet::new();
    let mut expected_dirs = std::collections::BTreeSet::new();
    for (name, record) in files {
        let rel = Path::new(name);
        if rel.as_os_str().is_empty()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid source path".to_owned());
        }
        expected_files.insert(rel.to_path_buf());
        let mut parent = rel.parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_dirs.insert(path.to_path_buf());
            parent = path.parent();
        }
        let mut path = root.clone();
        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            path.push(component.as_os_str());
            let meta =
                std::fs::symlink_metadata(&path).map_err(|e| format!("source {name}: {e}"))?;
            if components.peek().is_some() && (!meta.is_dir() || meta.file_type().is_symlink()) {
                return Err(format!("source parent is not a directory: {name}"));
            }
        }
        let kind = record["kind"]
            .as_str()
            .ok_or_else(|| "missing source kind".to_owned())?;
        let expected_file = field(record, "sha256", 64)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("source {name}: {e}"))?;
        let content = match kind {
            "file"
                if meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.len() <= 64 * 1024 * 1024 =>
            {
                let executable = record["executable"]
                    .as_bool()
                    .ok_or_else(|| format!("missing source mode: {name}"))?;
                if (meta.permissions().mode() & 0o111 != 0) != executable {
                    return Err(format!("source mode mismatch: {name}"));
                }
                read_bounded(&path, 64 * 1024 * 1024)?
            }
            "symlink" if meta.file_type().is_symlink() => std::fs::read_link(&path)
                .map_err(|e| format!("source {name}: {e}"))?
                .as_os_str()
                .as_bytes()
                .to_vec(),
            _ => return Err(format!("source kind or size mismatch: {name}")),
        };
        if crate::digest::sha256_hex(&content) != expected_file {
            return Err(format!("source digest mismatch: {name}"));
        }
    }
    let mut pending = vec![PathBuf::new()];
    let mut seen = 0_usize;
    while let Some(dir) = pending.pop() {
        for entry in
            std::fs::read_dir(root.join(&dir)).map_err(|e| format!("source listing: {e}"))?
        {
            let entry = entry.map_err(|e| format!("source entry: {e}"))?;
            seen = seen.saturating_add(1);
            if seen > 100_000 {
                return Err("source inventory exceeds its bound".to_owned());
            }
            let relative = dir.join(entry.file_name());
            let kind = entry.file_type().map_err(|e| format!("source type: {e}"))?;
            if kind.is_dir() && !kind.is_symlink() {
                if !expected_dirs.remove(&relative) {
                    return Err("unexpected source directory".to_owned());
                }
                pending.push(relative);
            } else if !expected_files.remove(&relative) {
                return Err("unexpected source entry".to_owned());
            }
        }
    }
    if !expected_files.is_empty() || !expected_dirs.is_empty() {
        return Err("source inventory is incomplete".to_owned());
    }
    if files.get("Cargo.lock").and_then(|v| v["sha256"].as_str()) != Some(lock)
        || files
            .get("rust-toolchain.toml")
            .and_then(|v| v["sha256"].as_str())
            != Some(toolchain)
    {
        return Err("lockfile or toolchain binding mismatch".to_owned());
    }
    Ok(format!("sha256:{expected}"))
}

/// Compare the fixed Barge source obligations in the provisional manifest
/// proposal with the independently verified receipt. This does not validate
/// the rest of the shared schema or establish observation coverage.
pub(crate) fn verify_manifest_binding(
    manifest: &[u8],
    receipt: &Path,
    expected: &str,
) -> Result<(), String> {
    use std::io::Read;
    let mut receipt_bytes = Vec::new();
    std::fs::File::open(receipt)
        .map_err(|e| format!("cannot reopen receipt: {e}"))?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut receipt_bytes)
        .map_err(|e| format!("cannot reread receipt: {e}"))?;
    if receipt_bytes.len() > 8 * 1024 * 1024
        || crate::digest::sha256_hex(&receipt_bytes) != expected
    {
        return Err("receipt changed while binding the manifest".to_owned());
    }
    let export: serde_json::Value = serde_json::from_slice(&receipt_bytes)
        .map_err(|e| format!("invalid export receipt: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_slice(manifest).map_err(|e| format!("invalid manifest: {e}"))?;
    let source = doc
        .get("source")
        .ok_or_else(|| "manifest source binding is missing".to_owned())?;
    if source["barge_pin"] != "4af9360bd09629e970c143080ca159de8eef2562"
        || source["barge_slice_commit"] != export["commit"]
        || source["staged_lock_sha256"] != export["lock_sha256"]
        || source["barge_executable_sha256"] != export["executable_sha256"]
    {
        return Err("manifest and staged source binding disagree".to_owned());
    }
    let version = source["barge_toolchain"].as_str().unwrap_or_default();
    if version != "1.97.1" && version != export["rustc_version"].as_str().unwrap_or_default() {
        return Err("manifest toolchain binding disagrees".to_owned());
    }
    let recorded = source["barge_files"]
        .as_object()
        .ok_or_else(|| "manifest Barge file binding is missing".to_owned())?;
    let exported = export["files"]
        .as_object()
        .ok_or_else(|| "export file binding is missing".to_owned())?;
    let expected_files: Vec<_> = exported
        .iter()
        .filter(|(path, _)| path.starts_with("crates/barge/src/"))
        .collect();
    if expected_files.is_empty()
        || expected_files.len() != recorded.len()
        || expected_files
            .iter()
            .any(|(path, entry)| recorded.get(*path) != entry.get("sha256"))
    {
        return Err("manifest Barge source files disagree with the export".to_owned());
    }
    Ok(())
}

/// Run one window: open the tape, call the actual `release::plan` with the
/// real platform object, and always write the transcript. Returns the
/// summary and the exit code.
pub(crate) fn run(
    manifest_bytes: &[u8],
    fixture_root: &Path,
    transcript_out: &Path,
) -> Result<RunSummary, String> {
    run_bound(
        manifest_bytes,
        fixture_root,
        transcript_out,
        "embedded-unverified",
    )
}

/// Run a window with the externally checked export binding. The embedded
/// exercise deliberately uses an unverified label and cannot qualify export.
pub(crate) fn run_bound(
    manifest_bytes: &[u8],
    fixture_root: &Path,
    transcript_out: &Path,
    source_binding: &str,
) -> Result<RunSummary, String> {
    run_with_observer(
        manifest_bytes,
        fixture_root,
        transcript_out,
        source_binding,
        &crate::os::platform(),
    )
}

// Test-only injection of explicit process facts; ordinary harness callers above
// always supply the real MacOs observer. All other platform facts stay real.
fn run_with_observer(
    manifest_bytes: &[u8],
    fixture_root: &Path,
    transcript_out: &Path,
    source_binding: &str,
    observer: &dyn ProcessObserver,
) -> Result<RunSummary, String> {
    let request = match parse_manifest(manifest_bytes) {
        Ok(request) => request,
        Err(reason) => {
            write_apparatus_refusal(transcript_out, Some(manifest_bytes), &reason)?;
            return Err(reason);
        }
    };
    let manifest_sha256 = crate::digest::sha256_hex(manifest_bytes);
    let account = FixtureAccount {
        home: fixture_root.to_path_buf(),
        state: fixture_root.join("state"),
        tool_state: fixture_root.join("Library/Application Support/oxide/state"),
        tool_cache: fixture_root.join("Library/Caches/oxide"),
    };
    let state_paths = match StatePaths::resolve(&account, Some(&account.state)) {
        Ok(paths) => paths,
        Err(error) => {
            let reason = format!("the fixture state directory does not resolve: {error}");
            write_apparatus_refusal(transcript_out, Some(manifest_bytes), &reason)?;
            return Err(reason);
        }
    };
    if let Err(error) = begin_window(WindowConfig {
        case: request.case.clone(),
        manifest_sha256,
        entrypoint_symbol: "barge::release::plan",
        source_binding: source_binding.to_owned(),
    }) {
        let reason = format!("the tape window cannot open: {error}");
        write_apparatus_refusal(transcript_out, Some(manifest_bytes), &reason)?;
        return Err(reason);
    }
    crate::tape::attach_manifest(manifest_bytes);
    crate::tape::begin_fixture(fixture_root);

    // The caller clock is a real reading; the tape records it as the window's
    // first clock row through state::now_ms's own leaf statement.
    let now_ms = crate::state::now_ms();
    let named_roots = request.named_roots.clone();
    let engine_request = ReleaseRequest {
        namespace: &request.namespace,
        area: &request.area,
        volume_uuid: &request.volume_uuid,
        now_ms,
        bounds: ReleaseBounds::design(),
        named_roots: &named_roots,
    };
    let platform = crate::os::platform();
    let dirs: &dyn DirOps = &platform;
    let facts: &dyn AbsenceVolume = &platform;
    let mounts: &dyn MountRoot = &platform;
    let outcome = crate::release::plan(
        &state_paths,
        dirs,
        facts,
        mounts,
        observer,
        &account,
        &engine_request,
    );
    let summary = match outcome {
        Ok(report) => {
            let exit_code = if report.needs_human() { 3 } else { 0 };
            let eligible = report.plan.eligible_count;
            let engine_truncated = report.plan.timed_out || report.plan.truncated;
            let result = match serde_json::to_value(&report) {
                Ok(result) => result,
                Err(error) => {
                    let reason = format!("the engine's report cannot be serialized: {error}");
                    let document = finish_apparatus(&format!(
                        "the engine completed with exit code {exit_code} but {reason}"
                    ))?;
                    write_transcript(transcript_out, &document)?;
                    return Err(reason);
                }
            };
            let recorded = finish(
                crate::tape::EngineOutcome {
                    status: "completed",
                    exit_code,
                    refusal_kind: None,
                    refusal_message: None,
                    refusal_detail: Vec::new(),
                    engine_truncated,
                },
                Some(&result),
            )?;
            let (read_rows, issues) = publish_capture(transcript_out, &recorded, manifest_bytes)?;
            RunSummary {
                status: "completed",
                exit_code,
                eligible: Some(eligible),
                read_rows,
                caps: (
                    CAP_READ_ROWS,
                    CAP_DOCUMENT_BYTES,
                    CAP_OCCURRENCES,
                    CAP_PROCESS_ROWS,
                ),
                issues,
            }
        }
        Err(error) => {
            let exit_code = error.kind().exit_code();
            let refusal_detail = error
                .detail()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let recorded = finish(
                crate::tape::EngineOutcome {
                    status: "refused",
                    exit_code,
                    refusal_kind: Some(error.kind().as_str().to_owned()),
                    refusal_message: Some(error.message().to_owned()),
                    refusal_detail,
                    engine_truncated: false,
                },
                None,
            )?;
            let (read_rows, issues) = publish_capture(transcript_out, &recorded, manifest_bytes)?;
            RunSummary {
                status: "refused",
                exit_code,
                eligible: None,
                read_rows,
                caps: (
                    CAP_READ_ROWS,
                    CAP_DOCUMENT_BYTES,
                    CAP_OCCURRENCES,
                    CAP_PROCESS_ROWS,
                ),
                issues,
            }
        }
    };
    Ok(summary)
}

/// Convert the recorder document, write the shared transcript, and judge it
/// with the shared parser and reconciler rules. An apparatus fallback is
/// written and reported, never passed off as a capture.
fn publish_capture(
    path: &Path,
    recorded: &str,
    manifest: &[u8],
) -> Result<(usize, Vec<String>), String> {
    if recorder_schema(recorded).as_deref() == Some(crate::tape::APPARATUS_SCHEMA) {
        let document: serde_json::Value =
            serde_json::from_str(recorded).map_err(|error| error.to_string())?;
        write_transcript(path, recorded)?;
        return Err(document["reason"]
            .as_str()
            .unwrap_or("the capture fell back to an apparatus diagnostic")
            .to_owned());
    }
    let wire = match crate::tape::shared_transcript(recorded) {
        Ok(wire) => wire,
        Err(error) => {
            let document: serde_json::Value =
                serde_json::from_str(recorded).map_err(|parse| parse.to_string())?;
            let case = document["case"].as_str().unwrap_or("").to_owned();
            let digest = document["manifest_sha256"].as_str().map(str::to_owned);
            let reason = format!("the recorder document could not be converted: {error}");
            write_transcript(
                path,
                &crate::tape::apparatus_document(&case, digest.as_deref(), &reason),
            )?;
            let recorder_path = recorder_sidecar(path);
            std::fs::write(&recorder_path, recorded)
                .map_err(|write| format!("the recorder document cannot be retained: {write}"))?;
            return Err(reason);
        }
    };
    write_transcript(path, &wire)?;
    let document: serde_json::Value =
        serde_json::from_str(&wire).map_err(|error| error.to_string())?;
    let read_rows = document["reads"].as_array().map_or(0, Vec::len);
    let issues = crate::tape::assess_shared(&wire, manifest)?.issues;
    Ok((read_rows, issues))
}

/// The recorder document's sidecar path: the transcript file's own name with
/// `.recorder.json` appended, so `barge.json` keeps its recorder evidence at
/// `barge.json.recorder.json`. A replacement extension would collide with a
/// real sibling and would not name the transcript it belongs to.
fn recorder_sidecar(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".recorder.json");
    path.with_file_name(name)
}

fn recorder_schema(document: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(document)
        .ok()
        .and_then(|value| value["schema"].as_str().map(str::to_owned))
}

/// Write an apparatus diagnostic when no engine ran. Untrusted manifest text is
/// never copied into the document; the reason is bounded by the caller.
pub(crate) fn write_apparatus_refusal(
    path: &Path,
    manifest: Option<&[u8]>,
    reason: &str,
) -> Result<(), String> {
    let case = manifest
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|document| document["case"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let digest = manifest.map(crate::digest::sha256_hex);
    write_transcript(
        path,
        &crate::tape::apparatus_document(&case, digest.as_deref(), reason),
    )
}

/// Write the transcript document to the caller's path.
fn write_transcript(path: &Path, transcript: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("the transcript directory cannot be created: {error}"))?;
    }
    std::fs::write(path, transcript)
        .map_err(|error| format!("the transcript cannot be written: {error}"))?;
    let document: serde_json::Value = serde_json::from_str(transcript)
        .map_err(|error| format!("the transcript cannot be inspected: {error}"))?;
    if document["schema"] != crate::tape::TRANSCRIPT_SCHEMA {
        // A distinct apparatus diagnostic is a real result, not a failed write.
        if document["schema"] == crate::tape::APPARATUS_SCHEMA {
            return Ok(());
        }
        return Err(format!(
            "the written document is neither a transcript nor an apparatus diagnostic; path={}",
            path.display()
        ));
    }
    crate::tape::validate_shared_transcript(transcript.as_bytes())?;
    if capture_exhausted(&document) {
        return Err(format!(
            "the transcript was truncated or a capture cap was exhausted; path={}; truncation={}; notes={}",
            path.display(),
            document["truncation"],
            document["notes"],
        ));
    }
    Ok(())
}

// These are the same refusal conditions used by the external harness and its
// embedded self-check. A bounded host result never qualifies capture.
fn capture_exhausted(document: &serde_json::Value) -> bool {
    let truncation = &document["truncation"];
    truncation["document_truncated"] == true
        || truncation["read_rows_truncated"] == true
        || truncation["occurrences_truncated"] == true
        || truncation["process_rows_truncated"] == true
        || truncation["classifications_truncated"] == true
}

// Copy outside the fixture before it is removed, but within its caller-owned
// run root.
fn retain_transcript(fixture: &EmbeddedFixture, source: &Path) -> Result<PathBuf, String> {
    let directory = fixture.dir.path();
    let parent = directory.parent().ok_or("the fixture has no run root")?;
    let name = directory.file_name().ok_or("the fixture has no name")?;
    let mut retained_name = name.to_os_string();
    retained_name.push(".transcript.json");
    let retained = parent.join(retained_name);
    std::fs::copy(source, &retained)
        .map_err(|error| format!("the transcript cannot be retained: {error}"))?;
    eprintln!("barge.tape.exercise/1: retained {}", retained.display());
    Ok(retained)
}

/// The declaration the D8 BASE fixture uses: Oxide's outputs area, released by
/// reference, excluded from the reserve.
pub(crate) fn base_declaration() -> AreaDeclaration {
    AreaDeclaration {
        namespace: "oxide".to_owned(),
        area: "outputs".to_owned(),
        folder: "oxide-outputs".to_owned(),
        overrides: Default::default(),
        absent: crate::area::AbsentPolicy::Fail {
            attach: "the build volume".to_owned(),
        },
        prune: PrunePolicy::ReleaseOnReference,
        reserve: ReserveParticipation::Excluded,
        outputs_grant: None,
    }
}

/// The real volume facts of the fixture volume root, refused on the boot
/// volume or a non-APFS volume: the exercise runs on a real selected volume.
pub(crate) fn fixture_volume_record(
    root: &Path,
    volume_root: &Path,
) -> Result<(crate::registry::VolumeRecord, PathBuf), String> {
    let platform = crate::os::platform();
    let identity = platform
        .identify(root)
        .map_err(|error| format!("the fixture volume does not identify: {error}"))?;
    if identity.boot_volume {
        return Err(
            "the fixture root sits on the boot volume; the exercise needs a real \
             selected volume, so point the run root at the Builds scratch"
                .to_owned(),
        );
    }
    if identity.filesystem != crate::os::FileSystem::Apfs {
        return Err("the fixture volume is not APFS".to_owned());
    }
    let Some(uuid) = identity.volume_uuid.clone() else {
        return Err("the fixture volume has no UUID".to_owned());
    };
    let mount = identity.mount_point.clone();
    let relative = volume_root
        .strip_prefix(&mount)
        .map_err(|_| "the fixture volume root is not under the mount point".to_owned())?
        .to_path_buf();
    let record = crate::registry::VolumeRecord {
        volume_uuid: uuid,
        mount_hint: Some(mount.clone()),
        availability: crate::registry::Availability::Permanent,
        durability: crate::registry::Durability::Expendable,
        root_dir: relative.display().to_string(),
        folders: crate::registry::FolderOverrides::none(),
        origin: crate::registry::RecordOrigin::Declared,
        scratch_grants: crate::registry::ScratchGrants::none(),
        space: None,
        generation: 1,
        registered_ms: 1_792_525_019_847,
    };
    Ok((record, mount))
}

/// One owned root created through the real platform, inside the fixture.
pub(crate) fn fixture_owned_root(
    state: &StatePaths,
    record: &crate::registry::VolumeRecord,
    declaration: &AreaDeclaration,
) -> Result<crate::area::OwnedRoot, String> {
    let platform = crate::os::platform();
    let resolved = crate::area::resolve(
        &platform,
        &platform,
        &platform,
        &platform,
        record,
        declaration,
        None,
    )
    .map_err(|error| format!("the fixture area does not resolve: {error}"))?;
    let observer = crate::testkit::FakeObserver::new();
    crate::area::create_root(
        state,
        &platform,
        &observer,
        &resolved,
        crate::area::RootSpec {
            kind: crate::area::RootKind::AreaRoot,
        },
    )
    .map_err(|error| format!("the fixture root cannot be created: {error}"))
}

/// Write the Oxide-side fixture: a settled registry row naming `root`, whose
/// checkout is positively absent, in the default pool's control directory.
pub(crate) fn fixture_oxide_records(
    account: &FixtureAccount,
    root: &crate::area::OwnedRoot,
    volume_uuid: &str,
) -> Result<(), String> {
    const CHECKOUT_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let pool = account.tool_cache.join("build-pool");
    let outputs = pool
        .join(crate::inspect::OXIDE_CONTROL_DIR)
        .join(crate::reference::OXIDE_OUTPUTS_DIR);
    std::fs::create_dir_all(&outputs)
        .map_err(|error| format!("the outputs folder cannot be created: {error}"))?;
    let registry_row = format!(
        r#"{{"checkout_id":"{CHECKOUT_ID}","checkout":"{}","receipt":"{}","member":"target","root":"{}","root_kind":"external","checkout_volume_uuid":"{volume_uuid}","recipe":"target-debug-v1","generation":1,"selected":false,"pending":false}}"#,
        account.home.join("checkout").display(),
        account.home.join("receipt.json").display(),
        root.path.display(),
    );
    std::fs::write(
        outputs.join(crate::reference::OXIDE_REGISTRY_FILE),
        format!(
            r#"{{"version":"{}","schema":2,"revision":1,"complete":true,"created_by":"oxide","rows":[{registry_row}]}}"#,
            crate::reference::OXIDE_REGISTRY_V2
        ),
    )
    .map_err(|error| format!("the registry cannot be written: {error}"))
}

// ---- retained fixture custody --------------------------------------------
//
// The no-argument embedded exercise removes its own fixture when it drops, so a
// success, an early return or a panic before a caller's census loses the whole
// directory. Retained custody is an explicit, source-owned opt-in that keeps
// the fixture from its creation: the caller's guard owns removal after its own
// census and this process never removes it (`docs/knowledge/contracts/o2-tape.md`,
// "Retained fixture custody").

/// The variable that opts the no-argument route into retained custody.
///
/// It is not a signal or a deletion grant: it is checked once, before any
/// fixture exists, and only together with the run root below.
pub(crate) const RETAINED_CUSTODY_ENV: &str = "BARGE_TAPE_CUSTODY";

/// The only value [`RETAINED_CUSTODY_ENV`] accepts.
pub(crate) const RETAINED_CUSTODY_VALUE: &str = "barge.tape.kept-fixture/1";

/// The marker the observation driver matches, unchanged from the retained
/// transcript line beside it.
pub(crate) const KEPT_FIXTURE_MARKER: &str = "barge.tape.exercise/1: kept fixture ";

/// The longest kept-fixture path one custody line may carry. The line is bounded
/// before a fixture is created, so an over-long path can never be announced.
pub(crate) const KEPT_FIXTURE_PATH_MAX_BYTES: usize = 512;

/// The longest directory name `testkit::TempDir` gives a fixture here: the
/// label, the process id, the sequence and the nanoseconds.
const FIXTURE_NAME_MAX_BYTES: usize = 96;

/// The manifest file name inside a kept fixture.
pub(crate) const KEPT_MANIFEST_FILE: &str = "manifest.json";

/// The label the no-argument route's fixture uses.
const REAL_HOST_LABEL: &str = "tape-eval-real-host";

/// Who removes one fixture directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Custody {
    /// The ordinary route: the fixture removes itself when it drops.
    Ordinary,
    /// Retained custody: the fixture is kept from creation, and the caller's
    /// guard — not this process — removes it after its own census.
    Retained,
}

/// One fixture directory and the authority that removes it.
#[derive(Debug)]
pub(crate) enum FixtureDir {
    /// Ordinary custody: self-removing while it is alive.
    TempDir(crate::testkit::TempDir),
    /// Retained custody: kept at creation; no process-side removal exists.
    Kept(PathBuf),
}

impl FixtureDir {
    /// The directory's path.
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::TempDir(dir) => dir.path(),
            Self::Kept(path) => path,
        }
    }
}

/// Create one fresh fixture directory under `root` with the requested custody.
///
/// With [`Custody::Retained`] the directory leaves automatic cleanup inside this
/// call, before the caller runs any fallible step: nothing after it can re-arm
/// removal.
pub(crate) fn create_fixture_dir(
    root: &crate::testkit::RunRoot,
    label: &str,
    custody: Custody,
) -> FixtureDir {
    let temp = crate::testkit::TempDir::within_run_root(root, label);
    match custody {
        Custody::Ordinary => FixtureDir::TempDir(temp),
        Custody::Retained => FixtureDir::Kept(temp.keep()),
    }
}

/// Create one retained fixture directory and name it on `out` before any
/// fallible setup runs. A path the custody line cannot carry is refused, and the
/// refusal names the directory that is already kept but unannounced.
pub(crate) fn create_announced_fixture(
    root: &crate::testkit::RunRoot,
    label: &str,
    out: &mut dyn Write,
) -> Result<FixtureDir, String> {
    let dir = create_fixture_dir(root, label, Custody::Retained);
    announce_kept_fixture(dir.path(), &root.path, out).map_err(|reason| {
        format!(
            "{reason}; the fixture directory {} is kept and was not announced",
            dir.path().display()
        )
    })?;
    Ok(dir)
}

/// The one bounded line that names a kept fixture, or the reason this path
/// cannot be named.
///
/// A path that is not UTF-8, holds a control character or surrounding
/// whitespace, is longer than the bound, is the run root itself or is not a
/// plain path inside it never becomes an apparent custody record. Nothing is
/// written here.
pub(crate) fn kept_fixture_line(fixture: &Path, root: &Path) -> Result<String, String> {
    let reported = fixture
        .to_str()
        .ok_or_else(|| "the kept fixture path is not UTF-8 and cannot be named".to_owned())?;
    if reported.is_empty() || reported.chars().any(char::is_control) || reported.trim() != reported
    {
        return Err(
            "the kept fixture path holds a character the custody line cannot carry".to_owned(),
        );
    }
    if reported.len() > KEPT_FIXTURE_PATH_MAX_BYTES {
        return Err(format!(
            "the kept fixture path is longer than the {KEPT_FIXTURE_PATH_MAX_BYTES}-byte custody line bound"
        ));
    }
    let inside = fixture
        .strip_prefix(root)
        .map_err(|_| "the kept fixture is not inside the caller's run root".to_owned())?;
    if inside.as_os_str().is_empty() {
        return Err("the kept fixture is the run root itself, not a fixture inside it".to_owned());
    }
    if !inside
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err("the kept fixture path is not a plain path inside the run root".to_owned());
    }
    Ok(format!("{KEPT_FIXTURE_MARKER}{reported}"))
}

/// Write exactly one kept-fixture line, and nothing when the path cannot be
/// named.
pub(crate) fn announce_kept_fixture(
    fixture: &Path,
    root: &Path,
    out: &mut dyn Write,
) -> Result<(), String> {
    let line = kept_fixture_line(fixture, root)?;
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|error| format!("the kept fixture line cannot be written: {error}"))
}

/// Resolve this invocation's fixture custody from the two values that can state
/// it. Nothing is created, read or written here.
///
/// * No opt-in is the ordinary route, whatever run root is named: a directory
///   name is never guard authority on its own.
/// * The exact opt-in value with no run root, an empty, relative, un-namable,
///   over-long or root-level path is a refusal, so no fixture is created.
/// * The exact opt-in value with a caller-supplied run root is retained
///   custody, in a root this process does not own.
pub(crate) fn retained_custody_from(
    opt_in: Option<&OsStr>,
    supplied_root: Option<&OsStr>,
) -> Result<Option<crate::testkit::RunRoot>, String> {
    let Some(opt_in) = opt_in else {
        return Ok(None);
    };
    if opt_in != OsStr::new(RETAINED_CUSTODY_VALUE) {
        return Err(format!(
            "{RETAINED_CUSTODY_ENV} does not carry the only value this route accepts \
             ({RETAINED_CUSTODY_VALUE}); no fixture was created"
        ));
    }
    let root = supplied_root
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "the retained custody mode requires this invocation's own run root in {}; \
                 no fixture was created",
                crate::testkit::SCRATCH_ROOT_ENV
            )
        })?;
    let path = Path::new(root);
    if !Path::new(root).is_absolute() {
        return Err(format!(
            "the retained custody run root {} is not an absolute path; no fixture was created",
            path.display()
        ));
    }
    if path.parent().is_none() {
        return Err(format!(
            "the retained custody run root {} is the filesystem root, not a caller-owned \
             directory; no fixture was created",
            path.display()
        ));
    }
    let reported = root.to_str().ok_or_else(|| {
        format!(
            "the retained custody run root {} is not UTF-8 and cannot name a fixture; no \
             fixture was created",
            path.display()
        )
    })?;
    if reported.chars().any(char::is_control) {
        return Err(
            "the retained custody run root holds a control character; no fixture was created"
                .to_owned(),
        );
    }
    if reported.len() + 1 + FIXTURE_NAME_MAX_BYTES > KEPT_FIXTURE_PATH_MAX_BYTES {
        return Err(format!(
            "the retained custody run root is longer than the budget that keeps its fixture \
             name inside the {KEPT_FIXTURE_PATH_MAX_BYTES}-byte custody line; no fixture was created"
        ));
    }
    Ok(Some(crate::testkit::RunRoot {
        path: path.to_path_buf(),
        owned: false,
    }))
}

/// Refuse a retained run root that is absent, a link or not a directory. The
/// caller names the directory it owns; this proves the name resolves to one.
pub(crate) fn verify_retained_root(path: &Path) -> Result<(), String> {
    let facts = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "the retained custody run root {} cannot be read: {error}",
            path.display()
        )
    })?;
    if facts.file_type().is_symlink() {
        return Err(format!(
            "the retained custody run root {} is a link; name the directory this invocation owns",
            path.display()
        ));
    }
    if !facts.is_dir() {
        return Err(format!(
            "the retained custody run root {} is not a directory",
            path.display()
        ));
    }
    Ok(())
}

/// This process's fixture custody, from the environment.
fn retained_custody() -> Result<Option<crate::testkit::RunRoot>, String> {
    let custody = retained_custody_from(
        std::env::var_os(RETAINED_CUSTODY_ENV).as_deref(),
        std::env::var_os(crate::testkit::SCRATCH_ROOT_ENV).as_deref(),
    )?;
    if let Some(root) = &custody {
        verify_retained_root(&root.path)?;
    }
    Ok(custody)
}

/// Publish the manifest bytes the fixture already holds inside it, before any
/// engine call. The kept document is the one the window runs, not a
/// reconstruction of it.
fn write_manifest(fixture: &Path, manifest: &[u8]) -> Result<(), String> {
    let path = fixture.join(KEPT_MANIFEST_FILE);
    std::fs::write(&path, manifest).map_err(|error| {
        format!(
            "the kept fixture's manifest cannot be written at {}: {error}",
            path.display()
        )
    })
}

/// Publish the manifest inside the kept fixture, then run the window.
///
/// Both steps are injected so the setup controls can prove the order — the
/// manifest exists before any engine call — without running `release::plan`. A
/// publication that fails never reaches the run.
pub(crate) fn published_then_run<P, R>(
    fixture: &Path,
    manifest: &[u8],
    transcript_out: &Path,
    publish: P,
    run: R,
) -> Result<RunSummary, String>
where
    P: FnOnce(&Path, &[u8]) -> Result<(), String>,
    R: FnOnce(&Path, &Path) -> Result<RunSummary, String>,
{
    publish(fixture, manifest)?;
    run(fixture, transcript_out)
}

/// The honest reason for a retained run that stopped before a manifest existed.
/// The partial fixture is kept and named; nothing fabricates its manifest.
fn missing_manifest_reason(fixture: &Path, reason: &str) -> String {
    format!(
        "{reason}; the fixture at {} is kept without a manifest: no manifest bytes were published",
        fixture.display()
    )
}

/// One built fixture: the directory this process owns, the manifest bytes and
/// the volume UUID the record and the manifest both name.
#[derive(Debug)]
pub(crate) struct EmbeddedFixture {
    /// The exclusively owned fixture directory, with the custody it was built
    /// under.
    pub(crate) dir: FixtureDir,
    /// The manifest bytes the window is run under.
    pub(crate) manifest: Vec<u8>,
    /// The fixture-owned root used by writer counterfactuals.
    pub(crate) root: PathBuf,
}

/// The owned setup every fixture shares: the exclusively owned directory, the
/// real volume record, the initialized Barge state, an optional declaration and
/// the owned root created through the real platform. Building it writes only
/// inside the fixture and opens no window.
pub(crate) struct FixtureSetup {
    /// The exclusively owned fixture directory, with the custody it was built
    /// under.
    pub(crate) dir: FixtureDir,
    /// The fixture tree the volume record names.
    pub(crate) volume_root: PathBuf,
    /// The real mount point of the fixture volume.
    pub(crate) mount: PathBuf,
    /// The real volume record, as registered.
    pub(crate) record: crate::registry::VolumeRecord,
    /// The fixture's explicit account facts.
    pub(crate) account: FixtureAccount,
    /// The fixture's Barge state paths.
    pub(crate) state_paths: StatePaths,
    /// The declaration that was written, when one was.
    pub(crate) declaration: Option<AreaDeclaration>,
    /// The owned root, when the fixture created one.
    pub(crate) root: Option<crate::area::OwnedRoot>,
}

impl FixtureSetup {
    /// The owned root, which every prepared case but the undeclared one has.
    pub(crate) fn owned_root(&self) -> Result<&crate::area::OwnedRoot, String> {
        self.root
            .as_ref()
            .ok_or_else(|| "the fixture has no owned root".to_owned())
    }

    /// The fixture directory.
    pub(crate) fn dir(&self) -> &Path {
        self.dir.path()
    }
}

/// Build the shared owned setup inside this invocation's run root.
pub(crate) fn build_fixture_setup(
    label: &str,
    declaration: Option<&AreaDeclaration>,
) -> Result<FixtureSetup, String> {
    build_fixture_setup_in(&crate::testkit::scratch_root(), label, declaration)
}

/// Build the shared owned setup inside `root`, which its caller owns.
pub(crate) fn build_fixture_setup_in(
    root: &crate::testkit::RunRoot,
    label: &str,
    declaration: Option<&AreaDeclaration>,
) -> Result<FixtureSetup, String> {
    finish_fixture_setup(
        create_fixture_dir(root, label, Custody::Ordinary),
        declaration,
    )
}

/// Every fallible setup step, over a directory that already exists and already
/// has its custody. Nothing here creates, arms or disarms the directory.
fn finish_fixture_setup(
    dir: FixtureDir,
    declaration: Option<&AreaDeclaration>,
) -> Result<FixtureSetup, String> {
    let volume_root = dir.path().join("volume-root");
    std::fs::create_dir_all(&volume_root)
        .map_err(|error| format!("the fixture volume root cannot be created: {error}"))?;
    let (record, mount) = fixture_volume_record(&volume_root, &volume_root)?;
    let account = FixtureAccount {
        home: dir.path().to_path_buf(),
        state: dir.path().join("state"),
        tool_state: dir.path().join("Library/Application Support/oxide/state"),
        tool_cache: dir.path().join("Library/Caches/oxide"),
    };
    let state_paths = StatePaths::resolve(&account, Some(&account.state))
        .map_err(|error| format!("the fixture state does not resolve: {error}"))?;
    state_paths
        .initialize()
        .map_err(|error| format!("the fixture state cannot be initialized: {error}"))?;
    let mut locks = crate::state::LockSet::with_deadline(
        state_paths.clone(),
        crate::state::Deadline::after(std::time::Duration::from_secs(5)),
    );
    let lock = locks
        .lock_registry()
        .map_err(|error| format!("the fixture registry lock cannot be taken: {error}"))?;
    crate::registry::RegistryWriter::new(&state_paths)
        .publish(
            &lock,
            &crate::registry::RegistryDocument::new(vec![record.clone()]),
        )
        .map_err(|error| format!("the fixture registry cannot be written: {error}"))?;
    drop(lock);
    let (declaration, root) = match declaration {
        Some(declaration) => {
            crate::area::declare(&state_paths, declaration, 1_000)
                .map_err(|error| format!("the fixture area cannot be declared: {error}"))?;
            let root = fixture_owned_root(&state_paths, &record, declaration)?;
            (Some(declaration.clone()), Some(root))
        }
        None => (None, None),
    };
    Ok(FixtureSetup {
        dir,
        volume_root,
        mount,
        record,
        account,
        state_paths,
        declaration,
        root,
    })
}

/// The manifest document's fixed, deliberately unverified skeleton: the source
/// block is a placeholder a real window replaces with the exported receipt's
/// binding, which `verify_manifest_binding` checks against the staged commit,
/// lockfile and executable before any run. `case` and the per-case fixture
/// state are the caller's to fill.
pub(crate) fn manifest_document(setup: &FixtureSetup, case: &str) -> serde_json::Value {
    let record = &setup.record;
    serde_json::json!({
        "schema": crate::tape::MANIFEST_SCHEMA,
        "case": case,
        "window": {"identity": "embedded-exercise", "start_monotonic_ns": 0, "end_monotonic_ns": 0},
        "source": {
            "oxide_commit": "embedded-unverified",
            "oxide_files": {},
            "cargo_lock_sha256": "0".repeat(64),
            "rust_toolchain_sha256": "0".repeat(64),
            "barge_pin": "4af9360bd09629e970c143080ca159de8eef2562",
            "barge_slice_commit": "embedded-unverified",
            "barge_files": {},
            "staged_lock_sha256": "0".repeat(64),
            "oxide_toolchain": "1.97.1",
            "barge_toolchain": "1.97.1",
            "oxide_executable_sha256": "0".repeat(64),
            "barge_executable_sha256": "0".repeat(64),
            "oxide_tree": "0".repeat(40),
        },
        "dynamic": {"volumes": [], "writers": [], "external_allowances": []},
        "mandatory_observations": [],
        "expected_effects": [],
        "expected": {"classification": "MATCH", "roots": []},
        "fixture": {
            "root": setup.dir.path().display().to_string(),
            "device": 0,
            "inode": 0,
            "mode": 0o700,
            "entries": [],
            "roots": [],
            "volume": {"uuid": record.volume_uuid.as_str(), "mount": setup.mount.display().to_string()},
        },
        "barge": {
            "volume_uuid": record.volume_uuid.as_str(),
            "volume_mount": setup.mount.display().to_string(),
            "namespace": "oxide",
            "area": "outputs",
            "named_roots": [],
        },
    })
}

/// Build the whole BASE-shaped fixture: real volume record, declaration, one
/// owned root, settled Oxide row, absent checkout, and the manifest naming
/// them. Setup writes only inside the fixture, before any window opens.
pub(crate) fn build_fixture(label: &str) -> Result<EmbeddedFixture, String> {
    build_fixture_with(
        &crate::testkit::scratch_root(),
        label,
        Custody::Ordinary,
        &mut std::io::sink(),
    )
}

/// Build the whole BASE-shaped fixture inside `root` with the requested custody.
///
/// Retained custody is armed and announced before the first fallible setup step,
/// so a setup error leaves the partial directory kept and already named.
fn build_fixture_with(
    root: &crate::testkit::RunRoot,
    label: &str,
    custody: Custody,
    out: &mut dyn Write,
) -> Result<EmbeddedFixture, String> {
    let dir = match custody {
        Custody::Ordinary => create_fixture_dir(root, label, Custody::Ordinary),
        Custody::Retained => create_announced_fixture(root, label, out)?,
    };
    let fixture_path = dir.path().to_path_buf();
    finish_base_fixture(dir).map_err(|reason| match custody {
        Custody::Ordinary => reason,
        Custody::Retained => missing_manifest_reason(&fixture_path, &reason),
    })
}

/// The fallible remainder of a BASE fixture build, over a directory whose
/// custody is already decided.
fn finish_base_fixture(dir: FixtureDir) -> Result<EmbeddedFixture, String> {
    let setup = finish_fixture_setup(dir, Some(&base_declaration()))?;
    let root = setup.owned_root()?.clone();
    fixture_oxide_records(&setup.account, &root, setup.record.volume_uuid.as_str())?;
    let manifest = serde_json::to_vec_pretty(&manifest_document(&setup, "CTL-0"))
        .map_err(|error| format!("the manifest cannot encode: {error}"))?;
    let root = root.path;
    Ok(EmbeddedFixture {
        dir: setup.dir,
        manifest,
        root,
    })
}

/// Exercise the unchanged real-host route, accepting only its actual engine
/// outcome or a recorded bounded capture refusal. Success means the self-check
/// classified that result, never that a truncated capture qualified.
///
/// The ordinary route is unchanged. Under the explicit retained-custody opt-in
/// the fixture is kept from creation, its path is announced before any fallible
/// setup, and the manifest bytes are published inside it before the window
/// runs; this process never removes it afterwards.
pub(crate) fn embedded_exercise() -> Result<(), String> {
    match retained_custody()? {
        None => {
            let fixture = build_fixture(REAL_HOST_LABEL)?;
            let transcript_out = fixture.dir.path().join("transcripts").join("barge.json");
            let outcome = run(&fixture.manifest, fixture.dir.path(), &transcript_out);
            classify_exercise(&fixture, outcome)
        }
        Some(root) => {
            let mut err = std::io::stderr();
            let fixture = build_fixture_with(&root, REAL_HOST_LABEL, Custody::Retained, &mut err)?;
            let transcript_out = fixture.dir.path().join("transcripts").join("barge.json");
            let outcome = published_then_run(
                fixture.dir.path(),
                &fixture.manifest,
                &transcript_out,
                write_manifest,
                |fixture_root, transcript_out| run(&fixture.manifest, fixture_root, transcript_out),
            );
            classify_exercise(&fixture, outcome)
        }
    }
}

/// Classify one embedded outcome from its transcript. A capture-cap refusal is
/// an honest bounded result; any other disagreement is an apparatus failure.
fn classify_exercise(
    fixture: &EmbeddedFixture,
    outcome: Result<RunSummary, String>,
) -> Result<(), String> {
    let transcript_out = fixture.dir.path().join("transcripts").join("barge.json");
    let retained = retain_transcript(fixture, &transcript_out)?;
    let bytes = std::fs::read(&retained).map_err(|error| error.to_string())?;
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if bytes.len() > CAP_DOCUMENT_BYTES || document["schema"] != crate::tape::TRANSCRIPT_SCHEMA {
        return Err("the real-host transcript violates its document contract".to_owned());
    }
    if capture_exhausted(&document) {
        let reason = match outcome {
            Err(reason) => reason,
            Ok(_) => return Err("an exhausted capture was not refused".to_owned()),
        };
        eprintln!("barge.tape.exercise/1: bounded refusal (not qualification): {reason}");
    } else {
        let summary = outcome?;
        let recorded = document["outcome"]["state"].as_str();
        if recorded != Some(summary.status) {
            return Err("the real-host engine outcome does not match its transcript".to_owned());
        }
        if summary.status == "refused" && document["outcome"]["exit_code"] != summary.exit_code {
            return Err("the real-host refusal exit code does not match its transcript".to_owned());
        }
        eprintln!(
            "barge.tape.exercise/1: engine status={} exit={}; {} completeness issue(s); capture remains unqualified",
            summary.status,
            summary.exit_code,
            summary.issues.len()
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::area::PrunePolicy;
    use crate::os::VolumeLocator;
    use crate::tape::{
        CAP_DOCUMENT_BYTES, CAP_OCCURRENCES, CAP_PROCESS_ROWS, CAP_READ_ROWS, ENGINE,
        TRANSCRIPT_SCHEMA,
    };
    use std::path::Path;

    /// Actual release::plan over real fixture files and explicit synthetic
    /// process facts, independent of how many processes the host currently has.
    /// This positive control must complete with its settled root eligible.
    #[test]
    fn the_harness_runs_its_embedded_fixture_end_to_end() {
        let fixture = build_fixture("tape-eval-e2e").expect("the fixture builds");
        let transcript_out = fixture.dir.path().join("transcripts").join("barge.json");
        let observer = crate::testkit::FakeObserver::new();
        let outcome = run_with_observer(
            &fixture.manifest,
            fixture.dir.path(),
            &transcript_out,
            "synthetic-process-fixture-unqualified",
            &observer,
        );
        retain_transcript(&fixture, &transcript_out).expect("retain the positive control");
        let summary = outcome.expect("the deterministic window runs");
        assert_eq!(summary.status, "completed", "the plan completes");
        assert_eq!(summary.exit_code, 0, "a complete reference set exits 0");
        assert_eq!(summary.eligible, Some(1), "the settled root is releasable");
        assert_eq!(
            summary.caps,
            (
                CAP_READ_ROWS,
                CAP_DOCUMENT_BYTES,
                CAP_OCCURRENCES,
                CAP_PROCESS_ROWS
            )
        );
        assert!(
            summary.read_rows > 0,
            "the window records real observation rows"
        );
        // Completeness is the shared implementation's own: the window origin is
        // unproven and no emitted row collides within its key.
        assert!(
            summary
                .issues
                .iter()
                .any(|issue| issue == "window origin and end unproven"),
            "{:?}",
            summary.issues
        );
        assert!(
            !summary
                .issues
                .iter()
                .any(|issue| issue.contains("collision")),
            "{:?}",
            summary.issues
        );
        let transcript =
            std::fs::read_to_string(&transcript_out).expect("the transcript is written");
        crate::tape::validate_shared_transcript(transcript.as_bytes())
            .expect("the actual emitted transcript follows the shared wire");
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["schema"], TRANSCRIPT_SCHEMA);
        assert_eq!(document["engine"], ENGINE);
        assert_eq!(document["case"], "CTL-0");
        assert_eq!(
            document["manifest_sha256"],
            crate::digest::sha256_hex(&fixture.manifest)
        );
        assert_eq!(document["outcome"]["state"], "completed");
        assert_eq!(document["outcome"]["result"]["preview"], true);
        assert_eq!(
            document["outcome"]["result"]["oxide"]["complete"], true,
            "the fixture's reference set reads completely"
        );
        let rows = document["reads"].as_array().expect("read rows");
        assert!(
            rows.len() >= 32,
            "the window tapes the leaf inventory: state, locks, enumeration, \
             volume, custody and absence leaves; saw {}",
            rows.len()
        );
        let caps = &document["truncation"];
        let exhausted = caps["read_rows_truncated"].as_bool().unwrap_or(false)
            || caps["process_rows_truncated"].as_bool().unwrap_or(false);
        // Real file/volume leaves remain taped; process facts are explicitly
        // supplied by FakeObserver rather than inferred from the host.
        let before_scan = [
            "state::read_bounded_inner",
            "registry::read",
            "area::read_directory",
            "area::read_holds",
            "state::StateLock::shared_if_present",
            "reference::read_registry",
            "reference::control_directory_with",
            "reference::pools_holding",
            "reference::BargeReferences::read",
            "pool_custody::read_document",
            "reference::read_receipts_with",
            "area::resolve_by",
            "area::VolumeRoot::open_with",
            "MacOs::list_names",
            "MacOs::mounts_for_uuid",
            "identity::read",
        ];
        // With bounded synthetic process facts the complete tail is mandatory.
        let after_scan = [
            "custody::take_custody_by",
            "reference::read_oxide_marker",
            "os::prove_absence",
        ];
        let sites: Vec<&str> = rows.iter().filter_map(|row| row["site"].as_str()).collect();
        for expected in before_scan {
            assert!(
                sites.iter().any(|site| site.contains(expected)),
                "the transcript must record {expected}; saw {sites:?}"
            );
        }
        assert!(
            !exhausted,
            "the synthetic process fixture fits the fixed caps"
        );
        assert!(!capture_exhausted(&document));
        for expected in after_scan {
            assert!(
                sites.iter().any(|site| site.contains(expected)),
                "the transcript must record {expected}; saw {sites:?}"
            );
        }
        assert!(
            !document["clocks"]
                .as_array()
                .expect("clock rows")
                .is_empty(),
            "the caller clock is bound"
        );
        assert_eq!(
            document["effects"].as_array().expect("effects").len(),
            0,
            "the read-only plan's STATIC effect set is empty"
        );
        assert!(
            document["recorded_holders"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "this plan observes no structured holder record"
        );
        assert!(
            document["fixture_capture"]["post"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty()),
            "the apparatus captures the owned fixture tree"
        );
        assert!(
            transcript.len() <= CAP_DOCUMENT_BYTES,
            "the transcript respects its document cap"
        );
    }

    #[test]
    fn the_real_host_exercise_retains_its_bounded_outcome() {
        embedded_exercise().expect("the real-host outcome is honestly classified");
    }

    #[test]
    fn held_unknown_and_bounded_fixture_writers_never_become_eligible() {
        use crate::testkit::ScanMode;
        for scan in [
            ScanMode::Open(vec![crate::os::OpenDescriptor {
                pid: 123,
                descriptor: 4,
                path: PathBuf::from("/synthetic-held-root"),
            }]),
            ScanMode::Unreadable,
            ScanMode::Bounded,
        ] {
            let fixture = build_fixture("tape-writer-counterfactual").expect("fixture");
            let transcript_out = fixture.dir.path().join("transcript.json");
            let mut observer = crate::testkit::FakeObserver::new();
            observer.scan = match scan {
                ScanMode::Open(mut descriptors) => {
                    descriptors[0].path = fixture.root.clone();
                    ScanMode::Open(descriptors)
                }
                other => other,
            };
            let outcome = run_with_observer(
                &fixture.manifest,
                fixture.dir.path(),
                &transcript_out,
                "synthetic-writer-counterfactual-unqualified",
                &observer,
            );
            retain_transcript(&fixture, &transcript_out).expect("retain counterfactual");
            let summary = outcome.expect("counterfactual window completes");
            assert_eq!(
                summary.eligible,
                Some(0),
                "no unknown or held root is eligible"
            );
        }
    }

    /// A refusal is an engine outcome: the transcript is still written, with
    /// the engine's own vocabulary and exit code, and carries no result.
    #[test]
    fn a_refused_window_still_writes_its_transcript() {
        let fixture = build_fixture("tape-eval-refusal").expect("the fixture builds");
        let document: serde_json::Value =
            serde_json::from_slice(&fixture.manifest).expect("the manifest parses");
        let mut rewritten = document.clone();
        rewritten["barge"]["area"] = serde_json::json!("never-declared");
        let manifest = serde_json::to_vec_pretty(&rewritten).expect("the manifest re-encodes");
        let transcript_out = fixture.dir.path().join("transcripts").join("refused.json");
        let summary = run(&manifest, fixture.dir.path(), &transcript_out).expect("the window runs");
        assert_eq!(summary.status, "refused");
        assert_eq!(summary.eligible, None);
        let transcript =
            std::fs::read_to_string(&transcript_out).expect("the refusal transcript is written");
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["outcome"]["state"], "refused");
        assert_eq!(
            document["outcome"]["exit_code"], 2,
            "an undeclared area exits 2"
        );
        assert_eq!(document["outcome"]["source"], "engine");
        assert!(
            document["outcome"]["reason"]
                .as_str()
                .expect("a refusal reason")
                .contains("never-declared"),
            "the engine's own refusal names the undeclared area"
        );
        assert!(
            document["outcome"].get("result").is_none(),
            "a refused outcome carries no engine result"
        );
    }

    /// A manifest missing its members records an apparatus refusal, not an
    /// invented engine outcome, and never a transcript.
    #[test]
    fn an_apparatus_error_writes_a_refusal() {
        let fixture = crate::testkit::TempDir::new("tape-eval-apparatus");
        let transcript_out = fixture.path().join("transcripts").join("none.json");
        let summary = run(b"{}", fixture.path(), &transcript_out);
        assert!(summary.is_err(), "an empty manifest is refused");
        let bytes = std::fs::read(&transcript_out).expect("refusal document");
        let document: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(document["schema"], crate::tape::APPARATUS_SCHEMA);
        assert_eq!(document["version"], crate::tape::APPARATUS_SCHEMA);
        assert_eq!(
            document["manifest_sha256"],
            crate::digest::sha256_hex(b"{}")
        );
        assert!(
            document["reason"]
                .as_str()
                .expect("a reason")
                .contains("manifest"),
            "the refusal names the manifest"
        );
        assert!(
            crate::tape::validate_shared_transcript(&bytes).is_err(),
            "an apparatus diagnostic is never a conforming transcript"
        );
    }

    #[test]
    fn a_failed_conversion_keeps_the_recorder_document_at_its_sidecar_name() {
        let fixture = crate::testkit::TempDir::new("tape-recorder-sidecar");
        let transcript_out = fixture.path().join("transcripts").join("barge.json");
        // A completed recorder document with no result object is not a capture:
        // the shared wire requires one, so the adapter must refuse it.
        let recorded = serde_json::json!({
            "schema": crate::tape::RECORDER_SCHEMA,
            "engine": "barge",
            "case": "SIDECAR",
            "manifest_sha256": "0".repeat(64),
            "entrypoint": {"symbol": "barge::release::plan", "source_binding": "control"},
            "window_elapsed_ns": 1,
            "outcome": {"status": "completed", "exit_code": 0},
            "observation_complete": false,
            "effects": [], "reads": [], "clocks": [], "recorded_holders": [],
            "fixture_capture": null, "notes": [],
            "caps": {"read_rows": 0, "document_bytes": 0, "occurrences_per_key": 64,
                "process_rows": 0, "dropped_rows": 0}
        })
        .to_string();
        let error = publish_capture(&transcript_out, &recorded, b"{}")
            .expect_err("a completed document without a result cannot convert");
        assert!(error.contains("could not be converted"), "{error}");
        let transcript: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&transcript_out).expect("apparatus document"))
                .expect("JSON");
        assert_eq!(transcript["schema"], crate::tape::APPARATUS_SCHEMA);
        assert_eq!(transcript["case"], "SIDECAR");
        let sidecar = fixture
            .path()
            .join("transcripts")
            .join("barge.json.recorder.json");
        let retained: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar).expect("the sidecar is written"))
                .expect("JSON");
        assert_eq!(retained["schema"], crate::tape::RECORDER_SCHEMA);
        assert_eq!(retained["case"], "SIDECAR");
        assert!(
            !fixture
                .path()
                .join("transcripts")
                .join("barge.recorder.json")
                .exists(),
            "the sidecar never replaces the transcript's extension"
        );
    }

    #[test]
    fn exhausted_capture_is_written_then_refused() {
        let fixture = crate::testkit::TempDir::new("tape-cap-refusal");
        let path = fixture.path().join("cap.json");
        crate::tape::begin_window(crate::tape::WindowConfig {
            case: "CAP".to_owned(),
            manifest_sha256: "0".repeat(64),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "control".to_owned(),
        })
        .expect("the window opens");
        for _ in 0..=CAP_OCCURRENCES {
            crate::tape::read_absent("state::read_bounded_inner", Path::new("/fixture/one"));
        }
        let recorded = crate::tape::finish(
            crate::tape::EngineOutcome {
                status: "completed",
                exit_code: 0,
                refusal_kind: None,
                refusal_message: None,
                refusal_detail: Vec::new(),
                engine_truncated: false,
            },
            Some(&serde_json::json!({})),
        )
        .expect("the recorder document is written");
        let wire = crate::tape::shared_transcript(&recorded).expect("the wire converts");
        let error = write_transcript(&path, &wire).expect_err("exhausted capture refuses");
        assert!(error.contains(&format!("path={}", path.display())));
        assert!(error.contains("occurrences_truncated"));
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("written transcript"))
                .expect("JSON");
        assert_eq!(written["truncation"]["occurrences_truncated"], true);
        assert_eq!(written["outcome"]["state"], "completed");
    }

    #[test]
    fn export_binding_refuses_missing_and_mismatched_receipts() {
        let fixture = crate::testkit::TempDir::new("tape-export-binding");
        let receipt = fixture.path().join("export.json");
        assert!(verify_export(&receipt, &"0".repeat(64), &"0".repeat(64)).is_err());
        std::fs::write(&receipt, b"{}").expect("receipt fixture");
        assert!(verify_export(&receipt, &"0".repeat(64), &"0".repeat(64)).is_err());
        assert!(
            verify_export(&receipt, &crate::digest::sha256_hex(b"{}"), &"0".repeat(64)).is_err()
        );
    }

    #[test]
    fn export_binding_checks_source_and_running_executable() {
        let fixture = crate::testkit::TempDir::new("tape-export-source");
        let source = fixture.path().join("source");
        std::fs::create_dir(&source).expect("source fixture");
        std::fs::write(source.join("Cargo.lock"), b"lock").expect("lock");
        std::fs::write(source.join("rust-toolchain.toml"), b"toolchain").expect("toolchain");
        std::fs::create_dir_all(source.join("scripts/tape")).expect("wrapper directory");
        std::fs::write(source.join("scripts/tape/private-wrapper.py"), b"wrapper")
            .expect("wrapper");
        let wrapper = crate::digest::sha256_hex(b"wrapper");
        let lock = crate::digest::sha256_hex(b"lock");
        let toolchain = crate::digest::sha256_hex(b"toolchain");
        let exe = std::fs::read(std::env::current_exe().expect("executable path"))
            .expect("executable bytes");
        let mut doc = serde_json::json!({
            "schema": "barge.tape.export/1", "commit": "a".repeat(40),
            "tree": "b".repeat(40), "executable_sha256": crate::digest::sha256_hex(&exe),
            "lock_sha256": lock, "toolchain_sha256": toolchain,
            "rustc_sha256": "c".repeat(64), "rustc_version": "rustc 1.97.1 example",
            "oxide_sha256": "d".repeat(64),
            "files": {
                "Cargo.lock": {"kind": "file", "sha256": lock, "executable": false},
                "rust-toolchain.toml": {"kind": "file", "sha256": toolchain, "executable": false},
                "scripts/tape/private-wrapper.py": {"kind": "file", "sha256": wrapper, "executable": false}
            }
        });
        let receipt = fixture.path().join("export.json");
        let bytes = serde_json::to_vec(&doc).expect("receipt JSON");
        std::fs::write(&receipt, &bytes).expect("receipt");
        let closure_path = fixture.path().join("export-closure.json");
        let mut closure = serde_json::json!({
            "schema": "barge.tape.export-closure/1", "status": "passed",
            "scope": "live-inventory-comparison-only",
            "receipt_sha256": crate::digest::sha256_hex(&bytes), "wrapper_sha256": wrapper,
            "live_before_sha256": "e".repeat(64), "live_after_sha256": "e".repeat(64)
        });
        let closure_bytes = serde_json::to_vec(&closure).expect("closure JSON");
        std::fs::write(&closure_path, &closure_bytes).expect("closure");
        let closure_digest = crate::digest::sha256_hex(&closure_bytes);
        assert!(verify_export(
            &receipt,
            &crate::digest::sha256_hex(&bytes),
            &closure_digest
        )
        .is_ok());
        assert!(verify_export(
            &receipt,
            &crate::digest::sha256_hex(&bytes),
            &"0".repeat(64)
        )
        .is_err());
        let lock_path = source.join("Cargo.lock");
        let original_permissions = std::fs::metadata(&lock_path)
            .expect("lock metadata")
            .permissions();
        let mut executable_permissions = original_permissions.clone();
        executable_permissions.set_mode(original_permissions.mode() | 0o100);
        std::fs::set_permissions(&lock_path, executable_permissions).expect("change fixture mode");
        assert!(verify_export(
            &receipt,
            &crate::digest::sha256_hex(&bytes),
            &closure_digest
        )
        .is_err());
        std::fs::set_permissions(&lock_path, original_permissions).expect("restore fixture mode");
        std::fs::write(source.join("unexpected"), b"extra").expect("extra source");
        assert!(verify_export(
            &receipt,
            &crate::digest::sha256_hex(&bytes),
            &closure_digest
        )
        .is_err());
        std::fs::remove_file(source.join("unexpected")).expect("remove fixture file");
        std::fs::write(source.join("Cargo.lock"), b"changed").expect("changed source");
        assert!(verify_export(
            &receipt,
            &crate::digest::sha256_hex(&bytes),
            &closure_digest
        )
        .is_err());
        std::fs::write(source.join("Cargo.lock"), b"lock").expect("restored source");
        doc["executable_sha256"] = "0".repeat(64).into();
        let changed = serde_json::to_vec(&doc).expect("changed receipt JSON");
        std::fs::write(&receipt, &changed).expect("changed receipt");
        closure["receipt_sha256"] = crate::digest::sha256_hex(&changed).into();
        let changed_closure = serde_json::to_vec(&closure).expect("changed closure");
        std::fs::write(&closure_path, &changed_closure).expect("changed closure file");
        assert!(verify_export(
            &receipt,
            &crate::digest::sha256_hex(&changed),
            &crate::digest::sha256_hex(&changed_closure)
        )
        .is_err());
    }

    #[test]
    fn manifest_binding_requires_the_exported_commit_and_each_source_file() {
        let fixture = crate::testkit::TempDir::new("tape-manifest-binding");
        let receipt = fixture.path().join("export.json");
        let commit = "a".repeat(40);
        let lock = "b".repeat(64);
        let executable = "c".repeat(64);
        let file = "d".repeat(64);
        let export = serde_json::json!({
            "commit": commit, "lock_sha256": lock, "executable_sha256": executable,
            "rustc_version": "rustc 1.97.1 example",
            "files": {"crates/barge/src/lib.rs": {"sha256": file}}
        });
        let bytes = serde_json::to_vec(&export).expect("receipt JSON");
        std::fs::write(&receipt, &bytes).expect("receipt");
        let mut manifest = serde_json::json!({"source": {
            "barge_pin": "4af9360bd09629e970c143080ca159de8eef2562",
            "barge_slice_commit": commit, "staged_lock_sha256": lock,
            "barge_executable_sha256": executable, "barge_toolchain": "1.97.1",
            "barge_files": {"crates/barge/src/lib.rs": file}
        }});
        let digest = crate::digest::sha256_hex(&bytes);
        assert!(verify_manifest_binding(
            &serde_json::to_vec(&manifest).unwrap(),
            &receipt,
            &digest
        )
        .is_ok());
        manifest["source"]["barge_files"]["crates/barge/src/lib.rs"] = "e".repeat(64).into();
        assert!(verify_manifest_binding(
            &serde_json::to_vec(&manifest).unwrap(),
            &receipt,
            &digest
        )
        .is_err());
        manifest["source"]["barge_files"]["crates/barge/src/lib.rs"] = file.into();
        manifest["source"]["barge_slice_commit"] = "f".repeat(40).into();
        assert!(verify_manifest_binding(
            &serde_json::to_vec(&manifest).unwrap(),
            &receipt,
            &digest
        )
        .is_err());
    }

    #[test]
    fn apparatus_refusal_bounds_untrusted_diagnostic_text() {
        let fixture = crate::testkit::TempDir::new("tape-eval-large-refusal");
        let output = fixture.path().join("refusal.json");
        let reason = "x".repeat(CAP_DOCUMENT_BYTES + 1);
        write_apparatus_refusal(&output, Some(b"{}"), &reason).expect("refusal is written");
        let bytes = std::fs::read(output).expect("refusal exists");
        assert!(bytes.len() < CAP_DOCUMENT_BYTES);
        let document: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(document["schema"], crate::tape::APPARATUS_SCHEMA);
        assert_eq!(
            document["reason_sha256"],
            crate::digest::sha256_hex(reason.as_bytes())
        );
        assert!(
            document["reason"]
                .as_str()
                .expect("a bounded reason")
                .ends_with("[truncated]"),
            "the reason text is bounded"
        );
    }

    /// The fixture volume facts come from the real platform, and the census
    /// finds exactly the fixture's mount.
    #[test]
    fn the_fixture_record_names_the_real_volume() {
        let fixture = crate::testkit::TempDir::new("tape-eval-volume");
        let volume_root = fixture.path().join("volume-root");
        std::fs::create_dir_all(&volume_root).expect("the fixture volume root");
        let (record, mount) =
            fixture_volume_record(&volume_root, &volume_root).expect("the record is real");
        let platform = crate::os::platform();
        let mounts = platform
            .mounts_for_uuid(&record.volume_uuid)
            .expect("the mount census runs");
        let found: Vec<PathBuf> = mounts
            .iter()
            .map(|mount| mount.as_path().to_path_buf())
            .collect();
        assert_eq!(found, vec![mount.clone()], "one real mount, the fixture's");
    }

    /// The parse accepts the strict `/2` manifest, its optional `named_roots`,
    /// and refuses mistyped members with precise reasons.
    #[test]
    fn the_manifest_parse_binds_its_members() {
        let fixture = build_fixture("tape-manifest-parse").expect("the fixture builds");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fixture.manifest).expect("the manifest parses");
        manifest["case"] = serde_json::json!("LEG-LISTED");
        manifest["barge"]["named_roots"] = serde_json::json!(["root-a"]);
        let request =
            parse_manifest(&serde_json::to_vec(&manifest).unwrap()).expect("the manifest parses");
        assert_eq!(request.case, "LEG-LISTED");
        assert_eq!(request.namespace, "oxide");
        assert_eq!(request.area, "outputs");
        assert_eq!(request.named_roots, vec!["root-a".to_owned()]);
        let mut wrong_schema = manifest.clone();
        wrong_schema["schema"] = "unknown".into();
        assert!(parse_manifest(&serde_json::to_vec(&wrong_schema).unwrap()).is_err());
        let mut too_many = manifest.clone();
        too_many["barge"]["named_roots"] = serde_json::json!(vec!["root-a"; 65]);
        assert!(parse_manifest(&serde_json::to_vec(&too_many).unwrap()).is_err());
        let broken = serde_json::json!({"case": "X"});
        assert!(parse_manifest(&serde_json::to_vec(&broken).unwrap()).is_err());
    }

    /// The fixture declaration is the contract's BASE declaration: released by
    /// reference and excluded from the reserve, so neither `maintain` nor the
    /// space path can ever act on it.
    #[test]
    fn the_base_declaration_is_release_on_reference() {
        let declaration = base_declaration();
        assert_eq!(declaration.namespace, "oxide");
        assert_eq!(declaration.area, "outputs");
        assert_eq!(declaration.prune, PrunePolicy::ReleaseOnReference);
        assert_eq!(
            declaration.reserve,
            crate::area::ReserveParticipation::Excluded
        );
    }

    /// The transcript binds the manifest digest through the window config.
    #[test]
    fn the_manifest_digest_is_the_crate_sha256() {
        let bytes = b"manifest bytes";
        begin_window(WindowConfig {
            case: "c".to_owned(),
            manifest_sha256: crate::digest::sha256_hex(bytes),
            entrypoint_symbol: "barge::release::plan",
            source_binding: "test".to_owned(),
        })
        .expect("the window opens");
        read_present(
            "state::read_bounded_inner",
            Path::new("/fixture/manifest"),
            None,
            bytes,
            None,
        );
        let transcript = finish(
            crate::tape::EngineOutcome {
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
        assert!(
            transcript.contains(&crate::digest::sha256_hex(bytes)),
            "the transcript binds the manifest digest"
        );
    }

    /// The fixture's registry and declaration are real bytes the real readers
    /// accept, written under the same exclusive lock the registry writer
    /// requires.
    #[test]
    fn the_fixture_setup_writes_only_inside_itself() {
        let fixture = build_fixture("tape-eval-setup").expect("the fixture builds");
        let root = fixture.dir.path();
        assert!(root.join("state/registry.json").is_file(), "the registry");
        assert!(
            root.join("state/areas/oxide/outputs.json").is_file(),
            "the declaration"
        );
        assert!(
            root.join("Library/Caches/oxide/build-pool/oxide-managed/outputs/registry.json")
                .is_file(),
            "the Oxide registry"
        );
        // Outside the fixture, nothing was touched: the run root holds the
        // whole fixture, and the state paths all resolve inside it.
        let account = FixtureAccount {
            home: root.to_path_buf(),
            state: root.join("state"),
            tool_state: root.join("Library/Application Support/oxide/state"),
            tool_cache: root.join("Library/Caches/oxide"),
        };
        let state_paths =
            StatePaths::resolve(&account, Some(&account.state)).expect("the state resolves");
        assert!(state_paths.root().starts_with(root));
    }

    /// One fresh directory this test owns, as a caller-owned run root: fixtures
    /// live inside it and it is removed with the test's own fixture, not by the
    /// code under control.
    fn caller_root(base: &crate::testkit::TempDir) -> crate::testkit::RunRoot {
        let path = base.join("run");
        std::fs::create_dir(&path).unwrap();
        crate::testkit::RunRoot { path, owned: false }
    }

    /// The fixture paths an announcement stream named, and nothing else.
    fn announced_paths(out: &[u8]) -> Vec<PathBuf> {
        std::str::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| PathBuf::from(line.strip_prefix(KEPT_FIXTURE_MARKER).unwrap()))
            .collect()
    }

    /// The steps one injected run recorded, in order.
    fn events<'a>(order: &std::cell::RefCell<Vec<&'a str>>) -> Vec<&'a str> {
        order.borrow().clone()
    }

    /// The two lifetime outcomes side by side: the ordinary fixture removes
    /// itself on a successful drop, and the retained one keeps every byte.
    /// Neither route runs an engine.
    #[test]
    fn custody_ordinary_fixture_removes_itself_and_retained_fixture_stays() {
        let base = crate::testkit::TempDir::new("custody-drop");
        let root = caller_root(&base);
        let ordinary = create_fixture_dir(&root, "custody-ordinary", Custody::Ordinary);
        let ordinary_path = ordinary.path().to_path_buf();
        assert!(ordinary_path.is_dir());
        drop(ordinary);
        assert!(
            !ordinary_path.exists(),
            "an ordinary fixture removes itself"
        );

        let retained = create_fixture_dir(&root, "custody-retained", Custody::Retained);
        assert!(matches!(retained, FixtureDir::Kept(_)));
        let kept_path = retained.path().to_path_buf();
        std::fs::write(kept_path.join("partial.txt"), b"left behind").unwrap();
        drop(retained);
        assert!(
            kept_path.is_dir(),
            "the retained fixture survives its own drop"
        );
        assert!(
            kept_path.join("partial.txt").is_file(),
            "its bytes are untouched"
        );
    }

    /// Retention is armed at creation, so the first fallible setup step — the
    /// real one, made to fail by a file holding its directory name — leaves the
    /// partial fixture kept and already named. The same failure under ordinary
    /// custody still cleans up, so default behaviour is unchanged.
    #[test]
    fn custody_retention_is_armed_before_the_first_fallible_setup_step() {
        let base = crate::testkit::TempDir::new("custody-setup");
        let root = caller_root(&base);

        let mut announced = Vec::new();
        let retained =
            create_announced_fixture(&root, "custody-setup-retained", &mut announced).unwrap();
        let kept = retained.path().to_path_buf();
        assert_eq!(announced_paths(&announced), vec![kept.clone()]);
        std::fs::write(kept.join("volume-root"), b"not a directory").unwrap();
        assert!(
            finish_fixture_setup(retained, None).is_err(),
            "the injected first fallible step fails"
        );
        assert!(
            kept.is_dir(),
            "the retained fixture survives the first fallible setup step"
        );
        assert!(
            kept.join("volume-root").is_file(),
            "the partial bytes are kept, not repaired"
        );

        let ordinary = create_fixture_dir(&root, "custody-setup-ordinary", Custody::Ordinary);
        let gone = ordinary.path().to_path_buf();
        std::fs::write(gone.join("volume-root"), b"not a directory").unwrap();
        assert!(finish_fixture_setup(ordinary, None).is_err());
        assert!(
            !gone.exists(),
            "an ordinary fixture still removes itself on the same failure"
        );
    }

    /// Unwinding is the same drop the lifetime decision already covers: the
    /// retained fixture stays and the ordinary one still goes.
    #[test]
    fn custody_panic_unwinding_keeps_the_retained_fixture() {
        let base = crate::testkit::TempDir::new("custody-panic");
        let root = caller_root(&base);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut retained_path = PathBuf::new();
        let retained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = create_fixture_dir(&root, "custody-panic-retained", Custody::Retained);
            retained_path = dir.path().to_path_buf();
            panic!("injected unwinding after creation");
        }));
        let mut ordinary_path = PathBuf::new();
        let ordinary = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = create_fixture_dir(&root, "custody-panic-ordinary", Custody::Ordinary);
            ordinary_path = dir.path().to_path_buf();
            panic!("injected unwinding after creation");
        }));
        std::panic::set_hook(previous);
        assert!(retained.is_err() && ordinary.is_err(), "both unwound");
        assert!(
            retained_path.is_dir(),
            "unwinding leaves the retained fixture in place"
        );
        assert!(
            !ordinary_path.exists(),
            "unwinding still cleans up an ordinary fixture"
        );
    }

    /// The kept path is named before any setup runs, in exactly one bounded
    /// line; a root whose path cannot be carried refuses and keeps the fixture
    /// unannounced rather than claiming custody it cannot report.
    #[test]
    fn custody_announcement_names_the_fixture_before_setup() {
        let base = crate::testkit::TempDir::new("custody-announce");
        let root = caller_root(&base);
        let mut out = Vec::new();
        let dir = create_announced_fixture(&root, "custody-announce-ok", &mut out).unwrap();
        let kept = dir.path().to_path_buf();
        assert!(kept.is_dir(), "the announced fixture exists");
        assert_eq!(announced_paths(&out), vec![kept]);
        assert_eq!(
            out.iter().filter(|byte| **byte == b'\n').count(),
            1,
            "exactly one line, and nothing else"
        );

        let bad_root = crate::testkit::RunRoot {
            path: base.path().join("bad\nroot"),
            owned: false,
        };
        std::fs::create_dir(&bad_root.path).unwrap();
        let mut refused = Vec::new();
        let failure =
            create_announced_fixture(&bad_root, "custody-announce-bad", &mut refused).unwrap_err();
        assert!(refused.is_empty(), "nothing was announced");
        assert!(
            failure.contains("kept"),
            "the refusal names the kept fixture"
        );
        assert_eq!(
            std::fs::read_dir(&bad_root.path).unwrap().count(),
            1,
            "the fixture is kept, not removed"
        );
    }

    /// The manifest is inside the kept fixture before any run call, and a
    /// publication that fails never reaches the run. The steps are injected, so
    /// no engine runs.
    #[test]
    fn custody_manifest_is_published_before_the_window_runs() {
        let base = crate::testkit::TempDir::new("custody-manifest");
        let root = caller_root(&base);
        let dir = create_fixture_dir(&root, "custody-manifest-run", Custody::Retained);
        let fixture = dir.path().to_path_buf();
        let transcript_out = fixture.join("transcripts").join("barge.json");
        let manifest = b"{\"schema\":\"oxide.capture.manifest/2\"}".to_vec();
        let order = std::cell::RefCell::new(Vec::<&str>::new());
        let stopped = published_then_run(
            &fixture,
            &manifest,
            &transcript_out,
            |fixture, bytes| {
                order.borrow_mut().push("publish");
                write_manifest(fixture, bytes)
            },
            |fixture, _transcript| {
                assert!(
                    events(&order) == vec!["publish"],
                    "the manifest is published first"
                );
                assert!(
                    fixture.join(KEPT_MANIFEST_FILE).is_file(),
                    "the manifest exists before any run call"
                );
                assert_eq!(
                    std::fs::read(fixture.join(KEPT_MANIFEST_FILE)).unwrap(),
                    manifest,
                    "the published bytes are the window's own"
                );
                order.borrow_mut().push("run");
                Err("injected engine stop".to_owned())
            },
        );
        assert_eq!(stopped.unwrap_err(), "injected engine stop");
        assert_eq!(events(&order), vec!["publish", "run"]);

        order.borrow_mut().clear();
        let refused = published_then_run(
            &fixture,
            &manifest,
            &transcript_out,
            |_, _| {
                order.borrow_mut().push("publish");
                Err("injected publication refusal".to_owned())
            },
            |_, _| {
                order.borrow_mut().push("run");
                Err("never reached".to_owned())
            },
        );
        assert_eq!(refused.unwrap_err(), "injected publication refusal");
        assert_eq!(
            events(&order),
            vec!["publish"],
            "a refused publication runs nothing"
        );
    }

    /// The opt-in and the run root are premises checked before any fixture
    /// exists: no opt-in is ordinary custody, and every absent, malformed,
    /// foreign, root-level or un-namable form refuses and creates nothing.
    #[test]
    fn custody_premises_refuse_before_any_fixture_exists() {
        use std::os::unix::ffi::OsStrExt;

        let exact = Some(OsStr::new(RETAINED_CUSTODY_VALUE));
        assert_eq!(
            retained_custody_from(None, None).unwrap(),
            None,
            "no opt-in is the ordinary route"
        );
        assert_eq!(
            retained_custody_from(None, Some(OsStr::new("/caller/run"))).unwrap(),
            None,
            "a run root alone is never guard authority"
        );

        let base = crate::testkit::TempDir::new("custody-premises");
        let root = caller_root(&base);
        let retained = retained_custody_from(exact, Some(root.path.as_os_str()))
            .unwrap()
            .expect("the opt-in with a caller root is retained custody");
        assert_eq!(retained.path, root.path);
        assert!(!retained.owned, "the caller's root is never ours to remove");

        let long = format!("/{}", "a".repeat(KEPT_FIXTURE_PATH_MAX_BYTES));
        let refusals: [(Option<&OsStr>, Option<&OsStr>); 9] = [
            (Some(OsStr::new("")), Some(root.path.as_os_str())),
            (
                Some(OsStr::new("barge.tape.kept-fixture/2")),
                Some(root.path.as_os_str()),
            ),
            (Some(OsStr::new("1")), Some(root.path.as_os_str())),
            (Some(OsStr::new("false")), Some(root.path.as_os_str())),
            (exact, None),
            (exact, Some(OsStr::new(""))),
            (exact, Some(OsStr::new("relative/run"))),
            (exact, Some(OsStr::new("/"))),
            (exact, Some(OsStr::new("/caller/run\nroot"))),
        ];
        for (opt_in, supplied) in refusals {
            assert!(
                retained_custody_from(opt_in, supplied).is_err(),
                "{opt_in:?} with {supplied:?} must refuse"
            );
        }
        assert!(
            retained_custody_from(exact, Some(OsStr::new(long.as_str()))).is_err(),
            "an over-long root cannot keep a namable fixture"
        );
        assert!(
            retained_custody_from(exact, Some(OsStr::from_bytes(b"/caller/\xff"))).is_err(),
            "a non-UTF-8 root cannot name its fixture"
        );
        assert_eq!(
            std::fs::read_dir(&root.path).unwrap().count(),
            0,
            "no refused premise created a fixture"
        );

        assert!(
            verify_retained_root(&root.path.join("absent")).is_err(),
            "a root that does not exist refuses"
        );
        let file = root.path.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        assert!(
            verify_retained_root(&file).is_err(),
            "a file is not a run root"
        );
        let link = root.path.join("a-link");
        std::os::unix::fs::symlink(&root.path, &link).unwrap();
        assert!(
            verify_retained_root(&link).is_err(),
            "a link is not the caller's own directory"
        );
        assert!(verify_retained_root(&root.path).is_ok());
    }

    /// One bounded, exact line: a control character, surrounding whitespace, an
    /// over-long path, a non-UTF-8 path, the run root itself or a path outside it
    /// never becomes an apparent custody record, and a refused path writes
    /// nothing.
    #[test]
    fn custody_kept_fixture_line_is_bounded_and_exact() {
        use std::os::unix::ffi::OsStrExt;

        let base = crate::testkit::TempDir::new("custody-line");
        let root = caller_root(&base);
        let fixture = root.path.join("barge-test-x");
        std::fs::create_dir(&fixture).unwrap();
        assert_eq!(
            kept_fixture_line(&fixture, &root.path).unwrap(),
            format!("{KEPT_FIXTURE_MARKER}{}", fixture.display())
        );

        for outside in [
            base.path().to_path_buf(),
            root.path.clone(),
            PathBuf::from("/elsewhere/fixture"),
            Path::new(OsStr::from_bytes(b"/fixture/\xff")).to_path_buf(),
        ] {
            assert!(
                kept_fixture_line(&outside, &root.path).is_err(),
                "{} is not a namable fixture inside the root",
                outside.display()
            );
        }
        let newline = root.path.join("bad\nname");
        std::fs::create_dir(&newline).unwrap();
        assert!(kept_fixture_line(&newline, &root.path).is_err());
        assert!(
            kept_fixture_line(&root.path.join("trailing "), &root.path).is_err(),
            "surrounding whitespace cannot survive the driver's strip"
        );
        let long = root.path.join("a".repeat(KEPT_FIXTURE_PATH_MAX_BYTES));
        assert!(
            kept_fixture_line(&long, &root.path).is_err(),
            "an over-long path is never announced"
        );

        let mut out: Vec<u8> = Vec::new();
        assert!(announce_kept_fixture(&long, &root.path, &mut out).is_err());
        assert!(out.is_empty(), "a path that cannot be named writes no line");
        announce_kept_fixture(&fixture, &root.path, &mut out).unwrap();
        assert_eq!(
            out,
            format!("{KEPT_FIXTURE_MARKER}{}\n", fixture.display()).as_bytes()
        );
    }

    /// A retained run that stops before a manifest exists is kept and named
    /// with an honest missing-manifest reason. Nothing fabricates one.
    #[test]
    fn custody_missing_manifest_reason_names_the_kept_fixture() {
        let reason =
            missing_manifest_reason(Path::new("/kept/fixture"), "the fixture volume is not APFS");
        assert!(
            reason.starts_with("the fixture volume is not APFS"),
            "the original reason is preserved verbatim"
        );
        assert!(
            reason.contains("/kept/fixture"),
            "the kept fixture is named"
        );
        assert!(reason.contains("kept without a manifest"));
        assert!(reason.contains("no manifest bytes were published"));
    }
}
