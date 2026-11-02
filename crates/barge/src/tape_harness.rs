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
//! The manifest is `oxide.capture.manifest/1`, owned by the orchestrator. The
//! harness reads exactly these members and binds the whole document's SHA-256;
//! every other member is the orchestrator's and is ignored here:
//!
//! - `case`: the case name carried into the transcript;
//! - `barge.volume_uuid`: the fixture volume's UUID, as registered;
//! - `barge.namespace`, `barge.area`: the planned area;
//! - `barge.named_roots`: optional array of explicitly named roots.
//!
//! A member that is missing or of the wrong shape is an apparatus error: the
//! process exits `2` with the reason on stderr and writes no transcript,
//! because there is no engine outcome to record. The engine's own refusals —
//! an undeclared area, an unregistered volume, an unreadable registry — are
//! engine outcomes: the transcript is written with `outcome.status` of
//! `refused`, the engine's own refusal vocabulary, and the exit code
//! `ErrorKind::exit_code` gives, and the process exits with that code.
//!
//! ## Staged provenance for the later X-S2 orchestration
//!
//! The orchestrator stages this exact commit's tree, builds the harness
//! offline from the committed `Cargo.lock` with default features only — no
//! feature is enabled, and the proof feature must stay off — through its own
//! Cargo front door (`cargo test --locked --no-run --test tape_eval` under the
//! pinned toolchain 1.97.1), and invokes the staged executable with the argv
//! contract `--fixture-root --manifest --transcript-out --case`. The binary
//! digests, toolchain version and lockfile digest are the orchestrator's to
//! verify before any window; nothing here reads the live pool, root,
//! registry or account.

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

/// Run one window: open the tape, call the actual `release::plan` with the
/// real platform object, and always write the transcript. Returns the
/// summary and the exit code.
pub(crate) fn run(
    manifest_bytes: &[u8],
    fixture_root: &Path,
    transcript_out: &Path,
) -> Result<RunSummary, String> {
    let request = parse_manifest(manifest_bytes)?;
    let manifest_sha256 = crate::digest::sha256_hex(manifest_bytes);
    let account = FixtureAccount {
        home: fixture_root.to_path_buf(),
        state: fixture_root.join("state"),
        tool_state: fixture_root.join("Library/Application Support/oxide/state"),
        tool_cache: fixture_root.join("Library/Caches/oxide"),
    };
    let state_paths = StatePaths::resolve(&account, Some(&account.state))
        .map_err(|error| format!("the fixture state directory does not resolve: {error}"))?;
    begin_window(WindowConfig {
        case: request.case.clone(),
        manifest_sha256,
        entrypoint_symbol: "barge::release::plan",
        source_binding: staged_binding(),
    })
    .map_err(|error| format!("the tape window cannot open: {error}"))?;

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
            let result = serde_json::to_value(&report)
                .map_err(|error| format!("the engine's report cannot be serialized: {error}"))?;
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

/// The source binding a staged run carries: this crate's own version, so the
/// orchestrator's digest verification ties the executable to the exact staged
/// source it was built from.
fn staged_binding() -> String {
    format!(
        "barge {} at {}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Write the transcript document to the caller's path.
fn write_transcript(path: &Path, transcript: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("the transcript directory cannot be created: {error}"))?;
    }
    std::fs::write(path, transcript)
        .map_err(|error| format!("the transcript cannot be written: {error}"))
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

    /// A manifest missing its members is an apparatus error: no transcript is
    /// written, because there is no engine outcome to record.
    #[test]
    fn an_apparatus_error_writes_no_transcript() {
        let fixture = crate::testkit::TempDir::new("tape-eval-apparatus");
        let transcript_out = fixture.path().join("transcripts").join("none.json");
        let summary = run(b"{}", fixture.path(), &transcript_out);
        assert!(summary.is_err(), "an empty manifest is refused");
        assert!(
            !transcript_out.exists(),
            "no transcript for an apparatus error"
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

    /// The parse accepts the manifest's optional `named_roots` and refuses
    /// mistyped members with precise reasons.
    #[test]
    fn the_manifest_parse_binds_its_members() {
        let manifest = serde_json::json!({
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
