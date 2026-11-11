//! The operation journal's proof-only driver: one case group per invocation.
//!
//! This executable exists for `scripts/native/journal-proof.py`, which stages
//! it from the `proof-journal` feature build of HEAD inside the guard's own
//! scratch and runs exactly one case group per process. It is not product
//! surface: no CLI verb routes here, and the example target declares
//! `required-features`, so no ordinary build compiles it.
//!
//! Each case group builds its own fixture inside the directory the caller
//! names (`BARGE_PROOF_CASE_DIR`, one fresh directory per case under the
//! guard's owned run root): the state directory is resolved explicitly from
//! that case directory through [`StatePaths::resolve`]'s override — never
//! from the account — and the operation journal is initialized and driven
//! through the actual store, lock and publication code
//! ([`begin_operation`], [`read_operations`], [`consult_operations`],
//! [`advance_operation`], [`inspect_operation_journal`]) on those fresh
//! private filesystem fixtures. The unit bindings are synthetic; no real
//! volume is ever touched. No HOME is exported or read: the journal surface
//! reads no account facts.
//!
//! Publication and watermark failures are injected through the feature-gated
//! endpoints ([`fail_next_document_publish`], [`fail_next_watermark_publish`]),
//! and every injection is labelled in the case's evidence. The endpoints arm a
//! refusal; they never write anything themselves.
//!
//! The verdict is one JSON document on stdout:
//! `{"case": "...", "status": "passed"|"failed", "injections": [...],
//! "detail": "..."}`. A failed case exits nonzero. Nothing here touches a real
//! account, a real drive, an install or an adoption.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use barge::error::ErrorKind;
use barge::operation::{
    begin_operation, consult_operations, fail_next_document_publish, fail_next_watermark_publish,
    init_operation_journal, inspect_operation_journal, proof, read_operations, BeginRequest, Begun,
    FinalizeSpec, FinalizeStatus, OperationPhase, OperationRecord, OperationUnit, ResumePolicy,
};
use barge::os::{platform, VolumeUuid};
use barge::state::{
    now_ms, publish_intent, read_intents, CaptureRecord, CaptureVerdict, ForeignCapture,
    IntentPhase, IntentRecord, IntentUnit, LockSet, PutBackOutcome, StatePaths, CAPTURE_PREFIX,
    QUARANTINE_PREFIX,
};

const UUID: &str = "a6ac3ed7-33ea-46b4-8082-7254cf291ed2";
const DIGEST: &str = "4d1f8d3f1d6bbbf19bb1d3ff5b6e3d5a2c4d6e7f8091a2b3c4d5e6f708192a3b";
const CALLER: &str = "oxide.outputs";
const HOOK: &str = "oxide.settle-outputs";
const INTENT_ROWS: usize = barge::contract::INTENT_ROWS;

fn main() -> ExitCode {
    let Some(case) = std::env::args().nth(1) else {
        eprintln!("name the case group to run: J1..J12");
        return ExitCode::from(ErrorKind::Usage.exit_code());
    };
    let mut injections = Vec::new();
    let outcome = match case.as_str() {
        "J1" => init_and_binding(&mut injections),
        "J2" => request_identity(&mut injections),
        "J3" => full_capacity_retry(&mut injections),
        "J4" => crash_before_record(&mut injections),
        "J5" => torn_watermark_tail(&mut injections),
        "J6" => gap_and_loss(&mut injections),
        "J7" => retained_terminals(&mut injections),
        "J8" => malformed_and_bounds(&mut injections),
        "J9" => directory_custody(&mut injections),
        "J10" => consult_and_legacy(&mut injections),
        "J11" => capture_evidence(&mut injections),
        "J12" => diagnostic(&mut injections),
        other => Err(format!("unknown case group `{other}`")),
    };
    let (status, detail) = match outcome {
        Ok(detail) => ("passed", detail),
        Err(detail) => ("failed", detail),
    };
    let document = serde_json::json!({
        "case": case,
        "status": status,
        "injections": injections,
        "detail": detail,
    });
    println!("{document}");
    if status == "passed" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(ErrorKind::UnknownState.exit_code())
    }
}

/// A failed step aborts the case group with its reason.
macro_rules! step {
    ($expression:expr, $message:literal) => {
        match $expression {
            Ok(value) => value,
            Err(error) => return Err(format!("{}: {error}", $message)),
        }
    };
}

fn expect(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        return Ok(());
    }
    Err(message.to_owned())
}

// ---------- fixture plumbing, all through the actual store ----------

struct Fixture {
    _root: PathBuf,
    state: StatePaths,
    uuid: VolumeUuid,
    dirs: barge::os::MacOs,
}

fn fixture(label: &str) -> Result<Fixture, String> {
    let case_dir = std::env::var_os("BARGE_PROOF_CASE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "BARGE_PROOF_CASE_DIR is not set".to_owned())?;
    let root = case_dir.join(label);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create the case root: {error}"))?;
    let state = StatePaths::resolve(
        &FixtureAccount { home: root.clone() },
        Some(&root.join("state")),
    )
    .map_err(|error| format!("the state directory does not resolve: {error}"))?;
    state
        .initialize()
        .map_err(|error| format!("the layout does not initialize: {error}"))?;
    Ok(Fixture {
        _root: root,
        state,
        uuid: VolumeUuid::parse(UUID)
            .map_err(|error| format!("the fixture UUID is invalid: {error}"))?,
        dirs: platform(),
    })
}

struct FixtureAccount {
    home: PathBuf,
}

impl barge::os::AccountFacts for FixtureAccount {
    fn account_home(&self) -> barge::error::Result<PathBuf> {
        Ok(self.home.clone())
    }
    fn state_root(&self) -> barge::error::Result<PathBuf> {
        Ok(self.home.join("state"))
    }
    fn tool_state_dir(&self, tool: &str) -> barge::error::Result<PathBuf> {
        Ok(self
            .home
            .join("Library/Application Support")
            .join(tool)
            .join("state"))
    }
    fn tool_cache_dir(&self, tool: &str) -> barge::error::Result<PathBuf> {
        Ok(self.home.join("Library/Caches").join(tool))
    }
}

fn unit(uuid: &VolumeUuid, inode: u64) -> OperationUnit {
    OperationUnit {
        volume_uuid: uuid.clone(),
        mount_hint: PathBuf::from("/Volumes/Fixture"),
        root_dir: String::new(),
        folder: "fixture/journal".to_owned(),
        parent: String::new(),
        name: format!("{inode:032x}"),
        kind: barge::unit::UnitKind::Directory,
        device: 16_777_219,
        inode,
        marker: Some(barge::unit::UnitMarker::new(
            barge::unit::MarkerFamily::OxideOutputs,
            "a".repeat(64),
        )),
        root_id: None,
        capture_name: None,
        grant: barge::unit::UnitGrant::delegation(barge::unit::MarkerFamily::OxideOutputs),
    }
}

fn finalize() -> FinalizeSpec {
    FinalizeSpec {
        kind: HOOK.to_owned(),
        version: "1".to_owned(),
        payload: "{\"fixture\":true}".to_owned(),
    }
}

fn delegated_request(uuid: &VolumeUuid, key: &str, inode: u64) -> BeginRequest {
    BeginRequest::delegated(CALLER, key, DIGEST, finalize(), unit(uuid, inode))
}

fn begin(f: &Fixture, request: BeginRequest) -> Result<Begun, barge::error::Error> {
    let mut locks = LockSet::new(f.state.clone());
    begin_operation(&f.dirs, &f.dirs, &f.state, &mut locks, request)
}

fn begin_result(
    f: &Fixture,
    request: BeginRequest,
) -> Result<Result<Begun, barge::error::Error>, String> {
    let mut locks = LockSet::new(f.state.clone());
    Ok(begin_operation(
        &f.dirs, &f.dirs, &f.state, &mut locks, request,
    ))
}

fn read(f: &Fixture) -> Result<barge::operation::JournalView, String> {
    read_operations(&f.dirs, &f.dirs, &f.state, INTENT_ROWS)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the journal read as unbound".to_owned())
}

fn write_token(f: &Fixture) -> Result<barge::state::IntentWrite, String> {
    let mut locks = LockSet::new(f.state.clone());
    let _volume = locks
        .lock_volume(&f.uuid)
        .map_err(|error| error.to_string())?;
    locks
        .begin_intent_write()
        .map_err(|error| error.to_string())
}

/// Write one crafted record fixture, bypassing the store's allocation, with
/// the private mode the custody boundary requires.
fn write_record(f: &Fixture, record: &OperationRecord) -> Result<(), String> {
    record.validate().map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    let path = f
        .state
        .operations_dir()
        .join(format!("{}.json", record.sequence));
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())
}

fn cancelled_record(uuid: &VolumeUuid, sequence: u64, inode: u64) -> OperationRecord {
    let mut bound = unit(uuid, inode);
    bound.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
    OperationRecord {
        schema: barge::contract::OPERATION_SCHEMA.to_owned(),
        sequence,
        recorded_ms: now_ms(),
        phase_ms: now_ms(),
        generation: 2,
        resume: ResumePolicy::Caller,
        caller: Some(CALLER.to_owned()),
        key: Some(format!("prune-run-{sequence}")),
        request_digest: Some(DIGEST.to_owned()),
        finalize: Some(finalize()),
        absence: None,
        finalize_status: FinalizeStatus::Released,
        unit: bound,
        phase: OperationPhase::Cancelled,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
    }
}

fn pending_record(
    uuid: &VolumeUuid,
    sequence: u64,
    key: &str,
    digest: &str,
    inode: u64,
) -> OperationRecord {
    let mut bound = unit(uuid, inode);
    bound.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
    OperationRecord {
        schema: barge::contract::OPERATION_SCHEMA.to_owned(),
        sequence,
        recorded_ms: now_ms(),
        phase_ms: now_ms(),
        generation: 1,
        resume: ResumePolicy::Caller,
        caller: Some(CALLER.to_owned()),
        key: Some(key.to_owned()),
        request_digest: Some(digest.to_owned()),
        finalize: Some(finalize()),
        absence: None,
        finalize_status: FinalizeStatus::Owed,
        unit: bound,
        phase: OperationPhase::Intent,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
    }
}

fn barge_pending_record(uuid: &VolumeUuid, sequence: u64, inode: u64) -> OperationRecord {
    let mut bound = unit(uuid, inode);
    bound.capture_name = Some(format!("{QUARANTINE_PREFIX}{sequence}"));
    OperationRecord {
        schema: barge::contract::OPERATION_SCHEMA.to_owned(),
        sequence,
        recorded_ms: now_ms(),
        phase_ms: now_ms(),
        generation: 1,
        resume: ResumePolicy::Barge,
        caller: None,
        key: None,
        request_digest: None,
        finalize: None,
        absence: None,
        finalize_status: FinalizeStatus::Owed,
        unit: bound,
        phase: OperationPhase::Intent,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
    }
}

fn write_full_pair(f: &Fixture, watermark: u64) -> Result<(), String> {
    let epoch = barge::operation::JournalEpoch {
        schema: barge::contract::JOURNAL_EPOCH_SCHEMA.to_owned(),
        creation_id: "abc12345".repeat(4),
        created_ms: now_ms(),
        watermark,
    };
    let identity = barge::operation::JournalIdentity {
        schema: barge::contract::JOURNAL_IDENTITY_SCHEMA.to_owned(),
        creation_id: epoch.creation_id.clone(),
        created_ms: epoch.created_ms,
    };
    let epoch_bytes =
        serde_json::to_vec_pretty(&epoch).map_err(|error| format!("encode: {error}"))?;
    let identity_bytes =
        serde_json::to_vec_pretty(&identity).map_err(|error| format!("encode: {error}"))?;
    let epoch_path = f.state.operations_dir().join("journal.json");
    let identity_path = f.state.journal_identity_path();
    std::fs::write(&epoch_path, epoch_bytes).map_err(|error| format!("write epoch: {error}"))?;
    std::fs::write(&identity_path, identity_bytes)
        .map_err(|error| format!("write identity: {error}"))?;
    set_private(&epoch_path)?;
    set_private(&identity_path)
}

// ---------- the case groups ----------

fn init_and_binding(_injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j1")?;
    // Each init call takes its own fresh lock set: the lock order is
    // per-acquisition, and a set that already holds the registry lock is
    // spent (the store refuses re-acquisition as a lock-order violation).
    step!(
        init_operation_journal(
            &f.dirs,
            &f.dirs,
            &f.state,
            &mut LockSet::new(f.state.clone())
        ),
        "init"
    );
    let view = read(&f)?;
    expect(
        view.epoch.watermark == 0,
        "a fresh journal accounts for nothing",
    )?;
    expect(
        view.epoch.creation_id == view.identity.creation_id,
        "the pair carries one creation id",
    )?;
    step!(
        init_operation_journal(
            &f.dirs,
            &f.dirs,
            &f.state,
            &mut LockSet::new(f.state.clone())
        ),
        "re-init"
    );
    let again = read(&f)?;
    expect(
        again.identity.creation_id == view.identity.creation_id,
        "a re-init is a no-op",
    )?;
    Ok(format!(
        "init, re-init no-op, creation id {}",
        view.identity.creation_id
    ))
}

fn request_identity(_injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j2")?;
    init(&f)?;
    let begun =
        begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let first = begun.record().clone();
    match begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())? {
        Begun::Already(existing) => expect(
            existing.sequence == first.sequence,
            "a same-key request binds the existing operation",
        )?,
        Begun::Began(_) => return Err("a same-key request allocated a second record".to_owned()),
    }
    let mut moved = delegated_request(&f.uuid, "prune-run-1", 4_242);
    moved.unit.inode = 9_999;
    expect(
        begin(&f, moved).is_err(),
        "a different unit under the same key refuses",
    )?;
    let mut second = delegated_request(&f.uuid, "prune-run-2", 4_243);
    second.request_digest = Some("6".repeat(64));
    expect(
        begin(&f, second).is_ok(),
        "a genuinely different unit and request begins",
    )?;
    Ok("idempotency, mismatch refusal and unit binding held".to_owned())
}

fn init(f: &Fixture) -> Result<(), String> {
    let mut locks = LockSet::new(f.state.clone());
    init_operation_journal(&f.dirs, &f.dirs, &f.state, &mut locks)
        .map_err(|error| error.to_string())
}

fn full_capacity_retry(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j3")?;
    // A valid full journal: 4094 retained cancellations, one pending
    // Barge-owned record, one pending delegated record, watermark 4096.
    for sequence in 1..=4_094_u64 {
        write_record(&f, &cancelled_record(&f.uuid, sequence, 4_242))?;
    }
    write_record(&f, &barge_pending_record(&f.uuid, 4_095, 7_001))?;
    write_record(
        &f,
        &pending_record(&f.uuid, 4_096, "prune-run-live", DIGEST, 7_002),
    )?;
    write_full_pair(&f, INTENT_ROWS as u64)?;
    let view = read(&f)?;
    expect(
        view.records.len() == INTENT_ROWS,
        "the full journal answers",
    )?;

    // The exact live request is already bound, at capacity, with no write.
    let begun = begin(&f, delegated_request(&f.uuid, "prune-run-live", 7_002))
        .map_err(|e| e.to_string())?;
    expect(
        begun.record().sequence == 4_096,
        "the full journal answers Already for the live request",
    )?;
    // The Barge-owned unit is idempotent at capacity too.
    let begun =
        begin(&f, BeginRequest::barge_owned(unit(&f.uuid, 7_001))).map_err(|e| e.to_string())?;
    expect(
        begun.record().sequence == 4_095,
        "the full journal answers Already for the bound unit",
    )?;
    // A mismatched request under the live key refuses as itself.
    let mut mismatched = delegated_request(&f.uuid, "prune-run-live", 7_002);
    mismatched.request_digest = Some("7".repeat(64));
    expect(
        begin_result(&f, mismatched)?.is_err(),
        "a mismatched request refuses at capacity",
    )?;
    // A genuinely new allocation refuses, and writes nothing.
    let view_before = read(&f)?;
    // A genuinely new allocation with its own distinct valid digest: a
    // digest shared with any existing row is a different question.
    let mut fresh = delegated_request(&f.uuid, "prune-run-fresh", 8_001);
    fresh.request_digest = Some("9".repeat(64));
    let error = begin_result(&f, fresh)?
        .err()
        .ok_or("a full journal must refuse")?;
    expect(
        error.kind() == ErrorKind::ResourceBound,
        "a new allocation at the cap refuses as a resource bound",
    )?;
    let view_after = read(&f)?;
    expect(
        view_after.records.len() == view_before.records.len()
            && view_after.epoch.watermark == view_before.epoch.watermark,
        "the refused allocation wrote nothing",
    )?;
    injections.push("none");
    Ok(format!(
        "capacity {} answers Already and refuses new work without a write",
        INTENT_ROWS
    ))
}

fn crash_before_record(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j4")?;
    init(&f)?;
    injections.push("fail_next_document_publish(ENOSPC)");
    fail_next_document_publish(std::io::Error::from_raw_os_error(28));
    expect(
        begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).is_err(),
        "the injected publish failure reports",
    )?;
    let view = read(&f)?;
    expect(view.records.is_empty(), "nothing happened")?;
    Ok("the injected failure left the journal unchanged".to_owned())
}

fn torn_watermark_tail(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j5")?;
    init(&f)?;
    injections.push("fail_next_watermark_publish(ENOSPC)");
    fail_next_watermark_publish(std::io::Error::from_raw_os_error(28));
    let begun =
        begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    expect(
        begun.record().sequence == 1,
        "the record is confirmed durable",
    )?;
    let view = read(&f)?;
    expect(
        view.epoch.watermark == 0 && view.tail_sequence == Some(1),
        "the verified allocation tail is named",
    )?;
    let mut second = delegated_request(&f.uuid, "prune-run-2", 4_243);
    second.request_digest = Some("6".repeat(64));
    let begun = begin(&f, second).map_err(|e| e.to_string())?;
    expect(
        begun.record().sequence == 2,
        "the tail reconciles before allocating",
    )?;
    let view = read(&f)?;
    expect(
        view.epoch.watermark == 2 && view.tail_sequence.is_none(),
        "gapless again",
    )?;
    Ok("one recoverable tail, reconciled by the next begin".to_owned())
}

fn gap_and_loss(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j6")?;
    init(&f)?;
    begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let mut second = delegated_request(&f.uuid, "prune-run-2", 4_243);
    second.request_digest = Some("6".repeat(64));
    begin(&f, second).map_err(|e| e.to_string())?;
    // An operator-fixture drop stands in for the loss the gap must prove.
    std::fs::remove_file(f.state.operations_dir().join("1.json"))
        .map_err(|error| format!("the fixture cannot drop a record: {error}"))?;
    expect(read(&f).is_err(), "a gap is unknown, never silence")?;

    // A record lost after removal, finalize owed: the same refusal.
    let f2 = fixture("j6b")?;
    init(&f2)?;
    let begun =
        begin(&f2, delegated_request(&f2.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let mut record = begun.record().clone();
    let write = write_token(&f2)?;
    for phase in [
        OperationPhase::Renamed,
        OperationPhase::MarkerLast,
        OperationPhase::Removed,
        OperationPhase::Deleted,
    ] {
        record = step!(
            proof::advance(&write, &f2.dirs, &f2.dirs, &f2.state, &record, phase),
            "the ladder advances"
        );
    }
    expect(record.finalization_owed(), "deleted is still owed")?;
    std::fs::remove_file(f2.state.operations_dir().join("1.json"))
        .map_err(|error| format!("the fixture cannot drop the record: {error}"))?;
    expect(read(&f2).is_err(), "the loss is a gap, never silence")?;
    injections.push("fixture record removal (labelled loss)");
    Ok("gaps refuse as unknown through both loss shapes".to_owned())
}

fn retained_terminals(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j7")?;
    init(&f)?;
    let begun =
        begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let record = begun.record().clone();
    let write = write_token(&f)?;
    // Skipping is refused: every phase is its own durable evidence.
    expect(
        proof::advance(
            &write,
            &f.dirs,
            &f.dirs,
            &f.state,
            &record,
            OperationPhase::Deleted,
        )
        .is_err(),
        "the ladder never skips to deleted",
    )?;
    let cancelled = step!(
        proof::advance(
            &write,
            &f.dirs,
            &f.dirs,
            &f.state,
            &record,
            OperationPhase::Cancelled
        ),
        "the pre-effect cancellation"
    );
    expect(
        cancelled.is_terminal() && !cancelled.binds_conflict(),
        "a cancellation is a retained terminal",
    )?;
    let view = read(&f)?;
    expect(view.records.len() == 1, "the cancelled record is retained")?;
    // A deleted record acks and freezes.
    let begun =
        begin(&f, BeginRequest::barge_owned(unit(&f.uuid, 4_243))).map_err(|e| e.to_string())?;
    let mut record = begun.record().clone();
    for phase in [
        OperationPhase::Renamed,
        OperationPhase::MarkerLast,
        OperationPhase::Removed,
        OperationPhase::Deleted,
    ] {
        record = step!(
            proof::advance(&write, &f.dirs, &f.dirs, &f.state, &record, phase),
            "the ladder advances"
        );
    }
    let settled = step!(
        proof::advance(
            &write,
            &f.dirs,
            &f.dirs,
            &f.state,
            &record,
            OperationPhase::Complete
        ),
        "the ack settles"
    );
    expect(
        settled.finalize_status == FinalizeStatus::Acknowledged && settled.is_terminal(),
        "the ack is the only exit from deleted",
    )?;
    injections.push("none");
    Ok("cancellation retained; adjacency held; ack settles and freezes".to_owned())
}

fn malformed_and_bounds(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j8")?;
    init(&f)?;
    let operations = f.state.operations_dir();
    std::fs::write(
        operations.join("1.json"),
        "{\"schema\": \"barge.operation/1\"",
    )
    .map_err(|error| format!("fixture write: {error}"))?;
    expect(read(&f).is_err(), "a malformed record refuses")?;
    let intent = serde_json::to_vec_pretty(&legacy_intent(&f.uuid)).map_err(|e| e.to_string())?;
    std::fs::write(operations.join("1.json"), intent)
        .map_err(|error| format!("fixture write: {error}"))?;
    expect(read(&f).is_err(), "a foreign format refuses")?;
    let moved = cancelled_record(&f.uuid, 1, 4_242);
    let bytes = serde_json::to_vec_pretty(&moved).map_err(|e| e.to_string())?;
    let path = operations.join("3.json");
    std::fs::write(&path, bytes).map_err(|error| format!("fixture write: {error}"))?;
    set_private(&path)?;
    expect(read(&f).is_err(), "a name mismatch refuses")?;
    std::fs::remove_file(operations.join("3.json")).map_err(|e| e.to_string())?;
    std::fs::write(operations.join("notes.txt"), "stray").map_err(|e| e.to_string())?;
    expect(read(&f).is_err(), "a stray entry refuses")?;
    std::fs::remove_file(operations.join("notes.txt")).map_err(|e| e.to_string())?;
    let path = operations.join("1.json");
    std::fs::write(&path, vec![b'a'; barge::contract::INTENT_CAP_BYTES + 1])
        .map_err(|e| e.to_string())?;
    set_private(&path)?;
    expect(read(&f).is_err(), "an over-cap document refuses")?;
    injections.push("fixture malformed/oversize documents (labelled)");
    Ok("malformed, foreign, mismatched, stray and over-cap refuse".to_owned())
}

fn set_private(path: &Path) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod: {error}"))
}

fn directory_custody(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j9")?;
    init(&f)?;
    begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    // The journal directory's name becomes a link to an empty substitute: the
    // custody boundary refuses before anything is followed or published.
    let substitute = f.state.root().join("substitute");
    std::fs::create_dir(&substitute).map_err(|e| e.to_string())?;
    let original = f.state.root().join("operations-original");
    std::fs::rename(f.state.operations_dir(), &original).map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(&substitute, f.state.operations_dir()).map_err(|e| e.to_string())?;
    expect(
        read(&f).is_err(),
        "a linked journal directory refuses reads",
    )?;
    expect(
        begin(&f, delegated_request(&f.uuid, "prune-run-2", 4_243)).is_err(),
        "a linked journal directory refuses begins",
    )?;
    let mut locks = LockSet::new(f.state.clone());
    expect(
        init_operation_journal(&f.dirs, &f.dirs, &f.state, &mut locks).is_err(),
        "a linked journal directory refuses init",
    )?;
    let substitute_entries = std::fs::read_dir(&substitute)
        .map_err(|e| e.to_string())?
        .count();
    expect(
        substitute_entries == 0,
        "nothing was published into the substitute",
    )?;
    // Restore the real directory and widen its mode: refused again.
    std::fs::remove_file(f.state.operations_dir()).map_err(|e| e.to_string())?;
    std::fs::rename(&original, f.state.operations_dir()).map_err(|e| e.to_string())?;
    let mut permissions = std::fs::metadata(f.state.operations_dir())
        .map_err(|e| e.to_string())?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(f.state.operations_dir(), permissions).map_err(|e| e.to_string())?;
    expect(
        read(&f).is_err(),
        "an exposed journal directory refuses reads",
    )?;
    // A missing journal directory reads as unbound and refuses writes.
    let f2 = fixture("j9b")?;
    std::fs::remove_dir(f2.state.operations_dir()).map_err(|e| e.to_string())?;
    expect(
        read_operations(&f2.dirs, &f2.dirs, &f2.state, INTENT_ROWS)
            .map_err(|e| e.to_string())?
            .is_none(),
        "a missing journal directory reads as unbound",
    )?;
    expect(
        begin(&f2, delegated_request(&f2.uuid, "prune-run-1", 4_242)).is_err(),
        "a missing journal directory refuses begins",
    )?;
    injections.push("fixture symlink substitute + mode widening (labelled)");
    Ok("linked, exposed and missing directories refuse; no substitute write".to_owned())
}

fn consult_and_legacy(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j10")?;
    init(&f)?;
    begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let held = f.state.clone();
    let (ready, started) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let mut locks = LockSet::new(held);
        let _lock = locks.lock_operations().expect("the holder locks");
        ready.send(()).expect("the holder signals");
        std::thread::sleep(Duration::from_millis(300));
    });
    started
        .recv()
        .map_err(|_| "the holder never locked".to_owned())?;
    let mut busy = LockSet::new(f.state.clone());
    let error = consult_operations(&f.dirs, &f.dirs, &f.state, &mut busy, INTENT_ROWS)
        .err()
        .ok_or("a busy journal must refuse the consult")?;
    expect(
        error.kind() == ErrorKind::Locked,
        "a busy consult refuses immediately",
    )?;
    holder
        .join()
        .map_err(|_| "the holder panicked".to_owned())?;
    let mut free = LockSet::new(f.state.clone());
    let view = consult_operations(&f.dirs, &f.dirs, &f.state, &mut free, INTENT_ROWS)
        .map_err(|e| e.to_string())?
        .ok_or("the free consult answers")?;
    expect(
        view.records.len() == 1,
        "the consult answers the authoritative read",
    )?;
    // Legacy separation: the shipped journal keeps its own record beside it.
    let write = write_token(&f)?;
    step!(
        publish_intent(&write, &f.state, &legacy_intent(&f.uuid)),
        "the shipped journal writes"
    );
    let intents = read_intents(&f.state, INTENT_ROWS).map_err(|e| e.to_string())?;
    expect(
        intents.records.len() == 1,
        "the shipped journal reads its own record",
    )?;
    let view = read(&f)?;
    expect(
        view.records.len() == 1,
        "the successor journal is unchanged",
    )?;
    injections.push("none");
    Ok("busy consult refuses without waiting; the journals never mix".to_owned())
}

fn capture_evidence(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j11")?;
    init(&f)?;
    let begun =
        begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let record = begun.record().clone();
    let write = write_token(&f)?;
    let capture = CaptureRecord {
        // A child capture names a private `.barge-del-<nonce>` leaf: the
        // CAPTURE_PREFIX plus a 32-hex-digit nonce, exactly what the capture
        // validator accepts. (The unit's capture_name keeps QUARANTINE_PREFIX:
        // the journal records the intent's own quarantine leaf there.)
        private: format!("{CAPTURE_PREFIX}{}", "c".repeat(32)),
        original: "child.txt".to_owned(),
        device: record.unit.device,
        inode: 77,
        kind: "file".to_owned(),
        verdict: CaptureVerdict::Observed,
        mode: Some(0o100_644),
    };
    let next = step!(
        proof::record_captures(
            &write,
            &f.dirs,
            &f.dirs,
            &f.state,
            &record,
            std::slice::from_ref(&capture)
        ),
        "the capture chain publishes"
    );
    expect(
        next.generation > record.generation,
        "the generation advances",
    )?;
    let foreign = ForeignCapture {
        private: capture.private.clone(),
        original: "child.txt".to_owned(),
        device: 1,
        inode: 78,
        kind: "file".to_owned(),
        captured: None,
        put_back: PutBackOutcome::Pending,
        reason: "the captured object was not the observed one".to_owned(),
    };
    step!(
        proof::record_foreign_capture(&write, &f.dirs, &f.dirs, &f.state, &next, &foreign),
        "the foreign capture publishes before its put-back"
    );
    let view = read(&f)?;
    expect(
        view.records[0].captures.len() == 1 && view.records[0].foreign_captures.len() == 1,
        "the evidence chain reads back",
    )?;
    injections.push("none");
    Ok("capture and foreign-capture evidence publish before effects".to_owned())
}

fn diagnostic(injections: &mut Vec<&'static str>) -> Result<String, String> {
    let f = fixture("j12")?;
    init(&f)?;
    begin(&f, delegated_request(&f.uuid, "prune-run-1", 4_242)).map_err(|e| e.to_string())?;
    let report = step!(
        inspect_operation_journal(&f.dirs, &f.dirs, &f.state, INTENT_ROWS),
        "the diagnosis reads"
    );
    expect(
        report.authoritative() && report.findings.is_empty(),
        "a healthy journal answers",
    )?;
    std::fs::remove_file(f.state.operations_dir().join("1.json"))
        .map_err(|error| format!("the fixture cannot drop the record: {error}"))?;
    let before = std::fs::read_dir(f.state.operations_dir())
        .map_err(|e| e.to_string())?
        .count();
    let report = step!(
        inspect_operation_journal(&f.dirs, &f.dirs, &f.state, INTENT_ROWS),
        "the diagnosis reads the gap"
    );
    expect(!report.authoritative(), "a proven gap blocks reads")?;
    let after = std::fs::read_dir(f.state.operations_dir())
        .map_err(|e| e.to_string())?
        .count();
    expect(before == after, "the diagnosis performs no write")?;
    injections.push("fixture record removal (labelled loss)");
    Ok("the diagnostic reports findings without repairing".to_owned())
}

/// The shipped `barge.intent/2` record, for the legacy-separation fixtures.
fn legacy_intent(uuid: &VolumeUuid) -> IntentRecord {
    let intent_id = format!("{:032x}", 7);
    IntentRecord {
        schema: barge::contract::INTENT_SCHEMA.to_owned(),
        intent_id: intent_id.clone(),
        volume_uuid: uuid.clone(),
        mount_hint: PathBuf::from("/Volumes/Fixture"),
        root_dir: String::new(),
        folder: "fixture/journal".to_owned(),
        unit: IntentUnit {
            parent: String::new(),
            name: format!("{:032x}", 8),
            kind: barge::unit::UnitKind::Directory,
            device: 16_777_219,
            inode: 4_242,
            marker: Some(barge::unit::UnitMarker::new(
                barge::unit::MarkerFamily::Barge,
                "b".repeat(64),
            )),
            root_id: None,
            capture_name: format!("{QUARANTINE_PREFIX}{intent_id}"),
            grant: barge::unit::UnitGrant::native_root(format!("{:032x}", 8)),
        },
        plan_digest: "c".repeat(64),
        phase: IntentPhase::Intent,
        generation: 1,
        recorded_ms: now_ms(),
        phase_ms: now_ms(),
        captures: Vec::new(),
        foreign_captures: Vec::new(),
        identity: None,
        absence: None,
        caller_intent_id: None,
        caller_request_digest: None,
    }
}
