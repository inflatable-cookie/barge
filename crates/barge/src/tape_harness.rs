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
//! `now_ms`, and always writes one `oxide.capture.transcript/1` document —
//! including on refusal. It records only; it feeds nothing back, copies no
//! policy, and never touches the production account resolution or the
//! `proof-synthetic-account` feature.
//!
//! ## Manifest fields this harness reads
//!
//! The manifest is `oxide.capture.manifest/1`, owned by the orchestrator. No
//! machine-readable schema is accepted yet: Oxide #240 proposes
//! `oxide-capture-manifest.schema.json` under the same semantic contract, and
//! that reviewed shape is the wire contract. The members below are this
//! harness's consumption requirements, not a canonical layout; the reads are
//! cfg(test)-only apparatus, so reconciling them to the reviewed schema is a
//! harness change and nothing production depends on them. The harness reads
//! exactly these members and binds the whole document's SHA-256; every other
//! member is the orchestrator's and is ignored here:
//!
//! - `case`: the case name carried into the transcript;
//! - `barge.volume_uuid`: the fixture volume's UUID, as registered;
//! - `barge.namespace`, `barge.area`: the planned area;
//! - `barge.named_roots`: optional array of explicitly named roots.
//!
//! A member that is missing or of the wrong shape is an apparatus error: the
//! process exits `2` and writes a provisional apparatus refusal document with
//! no invented engine outcome. The engine's own refusals —
//! an undeclared area, an unregistered volume, an unreadable registry — are
//! engine outcomes: the transcript is written with `outcome.status` of
//! `refused`, the engine's own refusal vocabulary, and the exit code
//! `ErrorKind::exit_code` gives, and the process exits with that code.
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
    begin_window, finish, read_present, WindowConfig, CAP_DOCUMENT_BYTES, CAP_OCCURRENCES,
    CAP_PROCESS_ROWS, CAP_READ_ROWS,
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

/// Parse the manifest members this harness consumes. Anything else in the
/// document is the orchestrator's; a missing or mistyped member is refused.
pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<ManifestRequest, String> {
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
    if member(&document, "schema")?.as_str() != Some("oxide.capture.manifest/1") {
        return Err("the manifest schema is not oxide.capture.manifest/1".to_owned());
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
    crate::tape::attach_obligations(manifest_bytes);
    crate::tape::attach_effect_expectations(manifest_bytes);

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
    let observer: &dyn ProcessObserver = &platform;
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
                    let document = crate::tape::finish_omitted(crate::tape::EngineOutcome {
                        status: "completed",
                        exit_code,
                        refusal_kind: None,
                        refusal_message: None,
                        refusal_detail: Vec::new(),
                        engine_truncated,
                    })?;
                    write_transcript(transcript_out, &document)?;
                    return Err(reason);
                }
            };
            write_transcript(
                transcript_out,
                &finish(
                    crate::tape::EngineOutcome {
                        status: "completed",
                        exit_code,
                        refusal_kind: None,
                        refusal_message: None,
                        refusal_detail: Vec::new(),
                        engine_truncated,
                    },
                    Some(&result),
                )?,
            )?;
            RunSummary {
                status: "completed",
                exit_code,
                eligible: Some(eligible),
                read_rows: 0,
                caps: (
                    CAP_READ_ROWS,
                    CAP_DOCUMENT_BYTES,
                    CAP_OCCURRENCES,
                    CAP_PROCESS_ROWS,
                ),
            }
        }
        Err(error) => {
            let exit_code = error.kind().exit_code();
            let refusal_detail = error
                .detail()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            write_transcript(
                transcript_out,
                &finish(
                    crate::tape::EngineOutcome {
                        status: "refused",
                        exit_code,
                        refusal_kind: Some(error.kind().as_str().to_owned()),
                        refusal_message: Some(error.message().to_owned()),
                        refusal_detail,
                        engine_truncated: false,
                    },
                    None,
                )?,
            )?;
            RunSummary {
                status: "refused",
                exit_code,
                eligible: None,
                read_rows: 0,
                caps: (
                    CAP_READ_ROWS,
                    CAP_DOCUMENT_BYTES,
                    CAP_OCCURRENCES,
                    CAP_PROCESS_ROWS,
                ),
            }
        }
    };
    Ok(summary)
}

/// Write a bounded provisional protocol document when no engine ran. Hashing
/// the diagnostic prevents untrusted manifest text from overflowing the cap.
pub(crate) fn write_apparatus_refusal(
    path: &Path,
    manifest: Option<&[u8]>,
    reason: &str,
) -> Result<(), String> {
    let mut document = serde_json::json!({
        "schema": crate::tape::TRANSCRIPT_SCHEMA,
        "engine": "barge",
        "manifest_sha256": manifest.map(crate::digest::sha256_hex),
        "outcome": { "status": "apparatus_refused", "exit_code": 2,
            "reason_sha256": crate::digest::sha256_hex(reason.as_bytes()) },
        "engine_ran": false,
        "observation_complete": false,
        "reads": [], "effects": [], "clocks": [],
        "caps": { "read_rows": 0, "document_bytes": 0,
            "occurrences_per_key": CAP_OCCURRENCES, "process_rows": 0,
            "dropped_rows": 0 }
    });
    loop {
        let bytes = serde_json::to_string(&document)
            .map_err(|error| format!("the apparatus refusal cannot be serialized: {error}"))?;
        if document["caps"]["document_bytes"].as_u64() == Some(bytes.len() as u64) {
            return write_transcript(path, &bytes);
        }
        document["caps"]["document_bytes"] = bytes.len().into();
    }
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
    let caps = &document["caps"];
    if document["outcome"]["refusal_kind"] == "document_cap"
        || caps["document_truncated"] == true
        || caps["read_rows_exhausted"] == true
        || caps["occurrences_exhausted"] == true
        || caps["process_rows_exhausted"] == true
        || caps["dropped_rows"].as_u64().unwrap_or(0) != 0
    {
        return Err("the transcript was truncated or a capture cap was exhausted".to_owned());
    }
    Ok(())
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

/// One built fixture: the directory this process owns, the manifest bytes and
/// the volume UUID the record and the manifest both name.
pub(crate) struct EmbeddedFixture {
    /// The exclusively owned fixture directory.
    pub(crate) dir: crate::testkit::TempDir,
    /// The manifest bytes the window is run under.
    pub(crate) manifest: Vec<u8>,
}

/// Build the whole BASE-shaped fixture: real volume record, declaration, one
/// owned root, settled Oxide row, absent checkout, and the manifest naming
/// them. Setup writes only inside the fixture, before any window opens.
pub(crate) fn build_fixture(label: &str) -> Result<EmbeddedFixture, String> {
    let dir = crate::testkit::TempDir::new(label);
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
    let declaration = base_declaration();
    crate::area::declare(&state_paths, &declaration, 1_000)
        .map_err(|error| format!("the fixture area cannot be declared: {error}"))?;
    let root = fixture_owned_root(&state_paths, &record, &declaration)?;
    fixture_oxide_records(&account, &root, record.volume_uuid.as_str())?;
    let manifest = serde_json::json!({
        "schema": "oxide.capture.manifest/1",
        "case": "CTL-0",
        "window": {"identity": "embedded-exercise"},
        "fixture": {"volume": {"uuid": record.volume_uuid.as_str(), "mount": mount.display().to_string()}},
        "barge": {
            "volume_uuid": record.volume_uuid.as_str(),
            "volume_mount": mount.display().to_string(),
            "namespace": "oxide",
            "area": "outputs",
            "named_roots": [],
        },
    });
    let manifest = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("the manifest cannot encode: {error}"))?;
    Ok(EmbeddedFixture { dir, manifest })
}

/// The embedded end-to-end exercise the exported executable runs with no
/// arguments: build the exclusively owned fixture, run one window through
/// [`run`] into the fixture's own transcripts directory, and return the
/// summary for the caller's exit code.
pub(crate) fn embedded_exercise() -> Result<RunSummary, String> {
    let fixture = build_fixture("tape-eval")?;
    let transcript_out = fixture.dir.path().join("transcripts").join("barge.json");
    run(&fixture.manifest, fixture.dir.path(), &transcript_out)
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

    /// The embedded end-to-end exercise: the actual harness `run` over an
    /// exclusively owned directory fixture on a real selected volume. The
    /// transcript binds the manifest, records the leaf reads, and the plan
    /// completes with the settled, absent-checkout root eligible.
    #[test]
    fn the_harness_runs_its_embedded_fixture_end_to_end() {
        let fixture = build_fixture("tape-eval-e2e").expect("the fixture builds");
        let transcript_out = fixture.dir.path().join("transcripts").join("barge.json");
        let summary =
            run(&fixture.manifest, fixture.dir.path(), &transcript_out).expect("the window runs");
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
        let transcript =
            std::fs::read_to_string(&transcript_out).expect("the transcript is written");
        let document: serde_json::Value =
            serde_json::from_str(&transcript).expect("the transcript parses");
        assert_eq!(document["schema"], TRANSCRIPT_SCHEMA);
        assert_eq!(document["engine"], ENGINE);
        assert_eq!(document["case"], "CTL-0");
        assert_eq!(
            document["manifest_sha256"],
            crate::digest::sha256_hex(&fixture.manifest)
        );
        assert_eq!(document["outcome"]["status"], "completed");
        assert_eq!(document["result"]["preview"], true);
        assert_eq!(
            document["result"]["oxide"]["complete"], true,
            "the fixture's reference set reads completely"
        );
        let rows = document["reads"].as_array().expect("read rows");
        assert!(
            rows.len() >= 32,
            "the window tapes the leaf inventory: state, locks, enumeration, \
             volume, custody and absence leaves; saw {}",
            rows.len()
        );
        let caps = &document["caps"];
        let exhausted = caps["read_rows_exhausted"].as_bool().unwrap_or(false)
            || caps["process_rows_exhausted"].as_bool().unwrap_or(false);
        // The leaves the plan reads before the custody scan's whole-host
        // descriptor walk. They always fit: the fixture's own reads are small.
        let before_scan = [
            "state::read_bounded_inner",
            "registry::read",
            "area::read_directory",
            "area::read_holds",
            "state::StateLock::shared_if_present",
            "MacOs::process_table",
            "macos::read_identity",
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
        // The leaves behind the custody scan: the custody verdict, the marker
        // reread under the root's lock and the absence proof. On a busy host
        // the scan's own per-fd leaf rows exhaust the fixed read-row cap —
        // one row per listed descriptor pair is what the leaf tape owes — and
        // the tail rows are honestly absent: the caps block records the
        // truncation, which is the contract's own behaviour, never a quiet
        // completion.
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
        if exhausted {
            assert!(
                caps["dropped_rows"].as_u64().unwrap_or(0) > 0,
                "an exhausted cap must record its dropped rows"
            );
        } else {
            for expected in after_scan {
                assert!(
                    sites.iter().any(|site| site.contains(expected)),
                    "the transcript must record {expected}; saw {sites:?}"
                );
            }
        }
        assert_eq!(caps["read_rows"], rows.len());
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
            transcript.len() <= CAP_DOCUMENT_BYTES,
            "the transcript respects its document cap"
        );
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
        assert_eq!(document["outcome"]["status"], "refused");
        assert_eq!(
            document["outcome"]["exit_code"], 2,
            "an undeclared area exits 2"
        );
        assert!(
            document["outcome"]["refusal_message"]
                .as_str()
                .expect("a refusal message")
                .contains("never-declared"),
            "the engine's own refusal names the undeclared area"
        );
        assert!(document.get("result").is_none());
    }

    /// A manifest missing its members records an apparatus refusal, not an
    /// invented engine outcome.
    #[test]
    fn an_apparatus_error_writes_a_refusal() {
        let fixture = crate::testkit::TempDir::new("tape-eval-apparatus");
        let transcript_out = fixture.path().join("transcripts").join("none.json");
        let summary = run(b"{}", fixture.path(), &transcript_out);
        assert!(summary.is_err(), "an empty manifest is refused");
        let bytes = std::fs::read(&transcript_out).expect("refusal document");
        let document: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(document["outcome"]["status"], "apparatus_refused");
        assert_eq!(document["engine_ran"], false);
        assert_eq!(document["observation_complete"], false);
    }

    #[test]
    fn exhausted_capture_is_written_then_refused() {
        let fixture = crate::testkit::TempDir::new("tape-cap-refusal");
        let path = fixture.path().join("cap.json");
        let document = serde_json::json!({
            "schema": crate::tape::TRANSCRIPT_SCHEMA,
            "outcome": { "status": "completed", "exit_code": 0 },
            "caps": { "occurrences_exhausted": true, "dropped_rows": 1 }
        });
        let bytes = serde_json::to_string(&document).expect("JSON");
        assert!(write_transcript(&path, &bytes).is_err());
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("written transcript"))
                .expect("JSON");
        assert_eq!(actual["outcome"]["status"], "completed");
        assert_eq!(actual["caps"]["dropped_rows"], 1);
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
        assert_eq!(
            document["outcome"]["reason_sha256"],
            crate::digest::sha256_hex(reason.as_bytes())
        );
        assert_eq!(document["engine_ran"], false);
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

    /// The parse accepts the manifest's optional `named_roots` and refuses
    /// mistyped members with precise reasons.
    #[test]
    fn the_manifest_parse_binds_its_members() {
        let manifest = serde_json::json!({
            "schema": "oxide.capture.manifest/1",
            "case": "LEG-LISTED",
            "barge": {
                "volume_uuid": "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
                "volume_mount": "/Volumes/Builds",
                "namespace": "oxide",
                "area": "outputs",
                "named_roots": ["root-a"],
            },
        });
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
}
