//! Source-bound fixture driver for the separately activated coordinator proof.
//!
//! This proof-only executable uses a private state directory and synthetic
//! account home supplied by the guarded runner. It never operates on an
//! existing registration or caller registry.

use std::path::PathBuf;
use std::process::ExitCode;

use barge::area::{self, OutputsGrant, PrunePolicy};
use barge::delete::{self, DeleteBounds, OperationReaffirmation};
use barge::digest::sha256_hex;
use barge::operation::{
    self, BeginRequest, DeclarationAuthority, FinalizationContext, FinalizeHook, FinalizeSpec,
    OperationPhase, OperationRecord, OperationUnit,
};
use barge::os::{platform, AbsenceVolume, AccountFacts, AncestorExpected, DirOps, VolumeUuid};
use barge::registry::{
    Availability, Durability, FolderOverrides, RecordOrigin, RegistryWriter, ScratchGrants,
    VolumeRecord,
};
use barge::state::{self, LockSet, StatePaths};
use barge::unit::{MarkerFamily, UnitGrant, UnitKind, UnitMarker};

const ROOT: &str = "coordinator-root";
const CHECKOUT_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn main() -> ExitCode {
    let Some(case) = std::env::args().nth(1) else {
        eprintln!("a coordinator proof case is required");
        return ExitCode::from(2);
    };
    let result = run_case(&case);
    let verdict = match &result {
        Ok(()) => serde_json::json!({"case": case, "status": "passed"}),
        Err(reason) => serde_json::json!({"case": case, "status": "failed", "detail": reason}),
    };
    println!("{verdict}");
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

struct Fixture {
    state: StatePaths,
    request: BeginRequest,
    root: PathBuf,
    declaration: area::AreaDeclaration,
    settlement: PathBuf,
}

impl Fixture {
    fn prepare(case: &str) -> Result<Self, String> {
        let required = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} is not set"))
        };
        let home = required("HOME")?;
        let state_dir = required("BARGE_STATE_DIR")?;
        let mount = required("BARGE_PROOF_VOLUME_MOUNT")?;
        let uuid_text = std::env::var("BARGE_PROOF_VOLUME_UUID").map_err(|e| e.to_string())?;
        let uuid = VolumeUuid::parse(&uuid_text).map_err(|e| e.to_string())?;
        let platform = platform();
        // This invokes the proof feature's private-home and guarded-scratch
        // validation before creating any fixture content.
        platform
            .tool_cache_dir("oxide")
            .map_err(|e| e.to_string())?;
        let mounted = platform
            .identify_directory(&platform.open_path(&mount).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if mounted.volume_uuid.as_ref() != Some(&uuid) {
            return Err("the mounted proof image does not match its UUID".to_owned());
        }
        let state = StatePaths::resolve(&platform, Some(&state_dir)).map_err(|e| e.to_string())?;
        state.initialize().map_err(|e| e.to_string())?;
        let folder = format!("coordinator-{case}");
        let output = mount.join(&folder);
        std::fs::create_dir(&output).map_err(|e| e.to_string())?;
        let volume = VolumeRecord {
            volume_uuid: uuid.clone(),
            mount_hint: Some(mount.clone()),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            root_dir: String::new(),
            folders: FolderOverrides::none(),
            origin: RecordOrigin::Declared,
            scratch_grants: ScratchGrants::none(),
            space: None,
            generation: 1,
            registered_ms: state::now_ms(),
        };
        let mut locks = LockSet::new(state.clone());
        let registry = locks.lock_registry().map_err(|e| e.to_string())?;
        RegistryWriter::new(&state)
            .publish(
                &registry,
                &barge::registry::RegistryDocument::new(vec![volume]),
            )
            .map_err(|e| e.to_string())?;
        drop(registry);
        let declaration = area::AreaDeclaration {
            namespace: barge::oxide_outputs::AREA_NAMESPACE.to_owned(),
            area: barge::oxide_outputs::AREA_NAME.to_owned(),
            folder: folder.clone(),
            overrides: Default::default(),
            absent: area::AbsentPolicy::Fail {
                attach: "the owned proof image".to_owned(),
            },
            prune: PrunePolicy::ReleaseOnReference,
            reserve: area::ReserveParticipation::Excluded,
            outputs_grant: Some(OutputsGrant::new("oxide", 1_000)),
        };
        let declared = area::declare(&state, &declaration, 1_000).map_err(|e| e.to_string())?;
        let root = output.join(ROOT);
        std::fs::create_dir(&root).map_err(|e| e.to_string())?;
        std::fs::write(root.join("payload"), b"private coordinator proof")
            .map_err(|e| e.to_string())?;
        let checkout_parent = home.join("checkouts");
        std::fs::create_dir(&checkout_parent).map_err(|e| e.to_string())?;
        let checkout = checkout_parent.join("checkout");
        let checkout_uuid = platform
            .identify_directory(
                &platform
                    .open_path(&checkout_parent)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?
            .volume_uuid
            .ok_or("checkout UUID missing")?;
        let marker = format!(
            r#"{{"version":"{}","checkout":"{}","checkout_id":"{CHECKOUT_ID}","label":"dev","recipe":"target-debug-v1","kind":"external","volume_uuid":"{}","checkout_volume_uuid":"{}","nonce":"{NONCE}","generation":1,"created_ms":1000}}"#,
            barge::reference::OXIDE_MARKER_V2,
            checkout.display(),
            uuid,
            checkout_uuid
        );
        std::fs::write(root.join(barge::reference::OXIDE_MARKER_FILE), &marker)
            .map_err(|e| e.to_string())?;
        let root_id = platform
            .open_path(&root)
            .map_err(|e| e.to_string())?
            .identity();
        let prefix = platform
            .volume_prefix(&checkout)
            .ok_or("checkout mount missing")?;
        let absence = barge::os::prove_absence(
            &platform,
            &platform,
            &checkout,
            AncestorExpected::Registered {
                uuid: &checkout_uuid,
                recorded_mount: &prefix,
            },
        )
        .map_err(|e| e.to_string())?;
        let absence =
            serde_json::from_value(serde_json::to_value(absence).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let grant = UnitGrant::delegation(MarkerFamily::OxideOutputs);
        let unit = OperationUnit {
            volume_uuid: uuid,
            mount_hint: mount,
            root_dir: String::new(),
            folder,
            parent: String::new(),
            name: ROOT.to_owned(),
            kind: UnitKind::Directory,
            device: root_id.device.get(),
            inode: root_id.inode,
            marker: Some(UnitMarker::new(
                MarkerFamily::OxideOutputs,
                sha256_hex(marker.as_bytes()),
            )),
            root_id: None,
            capture_name: None,
            grant: grant.clone(),
        };
        let mut request = BeginRequest::delegated(
            "proof.synthetic",
            "one",
            DIGEST,
            FinalizeSpec {
                kind: "proof.settle".to_owned(),
                version: "1".to_owned(),
                payload: "{}".to_owned(),
            },
            unit,
        );
        request.absence = Some(absence);
        request.declaration = Some(DeclarationAuthority {
            namespace: declared.namespace,
            area: declared.area,
            generation: declared.generation,
            grant,
        });
        let mut locks = LockSet::new(state.clone());
        operation::init_operation_journal(&platform, &platform, &state, &mut locks)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            state,
            request,
            root,
            declaration,
            settlement: home.join("synthetic-settlement.json"),
        })
    }

    fn begin(&self) -> Result<operation::Begun, String> {
        let platform = platform();
        delete::begin_checked_delegated(
            &self.state,
            &platform,
            &platform,
            &platform,
            self.request.clone(),
            DeleteBounds::default(),
        )
        .map_err(|e| e.to_string())
    }

    fn execute(&self) -> Result<(), String> {
        let platform = platform();
        let report = delete::execute_checked_delegated(
            &self.state,
            &platform,
            &platform,
            &platform,
            &self.request,
            &Reaffirm,
            DeleteBounds::default(),
        )
        .map_err(|e| e.to_string())?;
        if report.outcome != delete::DeleteOutcome::Deleted || self.root.exists() {
            return Err(format!(
                "the real delete did not positively complete: {report:?}"
            ));
        }
        Ok(())
    }

    fn record(&self) -> Result<OperationRecord, String> {
        let platform = platform();
        operation::checked_operation_state(&platform, &platform, &self.state, &self.request)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the operation disappeared".to_owned())
    }

    fn finalize(&self, hooks: &[&dyn FinalizeHook]) -> Result<OperationRecord, String> {
        let platform = platform();
        operation::finalize_operation(
            &platform,
            &platform,
            &self.state,
            "proof.synthetic",
            "one",
            hooks,
        )
        .map_err(|e| e.to_string())
    }
}

struct Reaffirm;
impl OperationReaffirmation for Reaffirm {
    fn reaffirm(&self, _: &OperationRecord) -> barge::error::Result<()> {
        Ok(())
    }
}

struct Hook<'a> {
    version: &'static str,
    fail: std::cell::Cell<bool>,
    calls: std::cell::Cell<u32>,
    fixture: &'a Fixture,
    guard: &'a std::sync::Mutex<()>,
}

impl FinalizeHook for Hook<'_> {
    fn kind(&self) -> &str {
        "proof.settle"
    }
    fn version(&self) -> &str {
        self.version
    }
    fn finalize(
        &self,
        context: &FinalizationContext,
        record: &OperationRecord,
    ) -> barge::error::Result<()> {
        self.calls.set(self.calls.get() + 1);
        synthetic_settle(self.fixture, self.guard, context, record)?;
        if self.fail.replace(false) {
            return Err(barge::error::Error::new(
                barge::error::ErrorKind::Io,
                "injected lost reply",
            ));
        }
        Ok(())
    }
}

fn hook<'a>(
    fixture: &'a Fixture,
    guard: &'a std::sync::Mutex<()>,
    version: &'static str,
    fail: bool,
) -> Hook<'a> {
    Hook {
        version,
        fail: std::cell::Cell::new(fail),
        calls: std::cell::Cell::new(0),
        fixture,
        guard,
    }
}

// This is synthetic caller domain state, not Barge progress. The caller holds
// its own publication guard, performs the fail-fast complete consult, and
// creates one durable entry binding the operation. Repetition observes exactly
// that entry; it never infers settlement from a missing domain object.
fn synthetic_settle(
    fixture: &Fixture,
    guard: &std::sync::Mutex<()>,
    context: &FinalizationContext,
    record: &OperationRecord,
) -> barge::error::Result<()> {
    let refuse =
        |detail: String| barge::error::Error::new(barge::error::ErrorKind::UnknownState, detail);
    let _guard = guard.lock().map_err(|e| refuse(e.to_string()))?;
    let platform = platform();
    let consult = operation::consult_settlement(&platform, &platform, &fixture.state, context)?;
    if let operation::SettlementConsult::Owed(other) = &consult {
        if !other.is_empty() {
            return Err(refuse("another pending operation exists".to_owned()));
        }
    }
    let entry = serde_json::to_vec(&serde_json::json!({
        "sequence": record.sequence, "caller": record.caller, "key": record.key,
        "request": record.request_digest, "unit": record.unit, "handler": record.finalize,
    }))
    .map_err(|e| refuse(e.to_string()))?;
    if matches!(consult, operation::SettlementConsult::AlreadyComplete) {
        if std::fs::read(&fixture.settlement).map_err(|e| refuse(e.to_string()))? != entry {
            return Err(refuse(
                "the acknowledged settlement entry differs".to_owned(),
            ));
        }
        return Ok(());
    }
    match std::fs::read(&fixture.settlement) {
        Ok(existing) => {
            if existing != entry {
                return Err(refuse(
                    "the settlement entry differs from this operation".to_owned(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            state::publish_atomic(&fixture.settlement, &entry)?;
        }
        Err(error) => return Err(refuse(error.to_string())),
    }
    Ok(())
}

fn finalize_case(fixture: &Fixture, case: &str) -> Result<(), String> {
    fixture.begin()?;
    fixture.execute()?;
    if fixture.record()?.phase != OperationPhase::Deleted {
        return Err("the successor journal did not retain deletion".to_owned());
    }
    let guard = std::sync::Mutex::new(());
    let good = hook(fixture, &guard, "1", case == "lost-ack");
    if case == "owed" {
        if fixture.finalize(&[]).is_ok()
            || fixture
                .finalize(&[&hook(fixture, &guard, "2", false)])
                .is_ok()
            || fixture
                .finalize(&[&hook(fixture, &guard, "1", true)])
                .is_ok()
            || fixture.record()?.phase != OperationPhase::Deleted
        {
            return Err(
                "a missing, unsupported or failing hook lost the owed operation".to_owned(),
            );
        }
    }
    if case == "lost-ack" {
        if fixture.finalize(&[&good]).is_ok()
            || !fixture.settlement.exists()
            || fixture.record()?.phase != OperationPhase::Deleted
        {
            return Err("the lost reply did not preserve the owed operation".to_owned());
        }
    }
    let done = fixture.finalize(&[&good])?;
    if done.phase != OperationPhase::Complete || !fixture.settlement.exists() {
        return Err("the checked handler did not settle the operation".to_owned());
    }
    if case == "lost-ack" && good.calls.get() != 2 {
        return Err("the lost reply was not dispatched exactly twice".to_owned());
    }
    let legacy = state::read_intents(&fixture.state, barge::contract::INTENT_ROWS)
        .map_err(|e| e.to_string())?;
    if !legacy.records.is_empty() {
        return Err("a second legacy intent was published".to_owned());
    }
    Ok(())
}

fn run_case(case: &str) -> Result<(), String> {
    if !matches!(
        case,
        "positive"
            | "owed"
            | "lost-ack"
            | "concurrent"
            | "busy"
            | "held-writer"
            | "cancel"
            | "withdraw"
            | "mismatch"
            | "intent"
            | "intent-capture"
            | "renamed-replay"
            | "unlink-window"
            | "removed"
    ) {
        return Err(format!("case {case} is not yet implemented"));
    }
    let fixture = Fixture::prepare(case)?;
    match case {
        "positive" | "owed" | "lost-ack" => finalize_case(&fixture, case),
        "busy" => {
            fixture.begin()?;
            let platform = platform();
            let mut holder = LockSet::new(fixture.state.clone());
            let _lock = holder.lock_operations().map_err(|e| e.to_string())?;
            let mut observer = LockSet::new(fixture.state.clone());
            let error = operation::consult_operations(
                &platform,
                &platform,
                &fixture.state,
                &mut observer,
                barge::contract::INTENT_ROWS,
            )
            .expect_err("busy journal");
            if error.kind() != barge::error::ErrorKind::Locked {
                return Err(format!("busy journal returned {error}"));
            }
            Ok(())
        }
        "concurrent" => concurrent_case(&fixture),
        "held-writer" => {
            use std::io::Write as _;
            let payload = fixture.root.join("payload");
            println!(
                "{}",
                serde_json::json!({"case":case, "stage":"writer-ready", "payload":payload})
            );
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            let mut signal = String::new();
            std::io::stdin()
                .read_line(&mut signal)
                .map_err(|e| e.to_string())?;
            if signal.trim() != "writer-open" {
                return Err("the supervisor did not bind the writer".to_owned());
            }
            let error = fixture.begin().expect_err("a real open writer must refuse");
            if !error.contains("holds descriptor") || !payload.exists() {
                return Err(format!(
                    "the refusal did not identify the open writer: {error}"
                ));
            }
            Ok(())
        }
        "cancel" => {
            fixture.begin()?;
            let platform = platform();
            let result = delete::cancel_delegated_operation(
                &fixture.state,
                &platform,
                &platform,
                &platform,
                &fixture.request,
            )
            .map_err(|e| e.to_string())?;
            if result.phase != OperationPhase::Cancelled || !fixture.root.join("payload").exists() {
                return Err("cancellation did not retain its no-effect terminal".to_owned());
            }
            Ok(())
        }
        "withdraw" => {
            fixture.begin()?;
            let mut declaration = fixture.declaration.clone();
            declaration.outputs_grant = None;
            area::declare(&fixture.state, &declaration, 2_000).map_err(|e| e.to_string())?;
            if fixture.execute().is_ok()
                || !fixture.root.join("payload").exists()
                || fixture.record()?.phase != OperationPhase::Intent
            {
                return Err("withdrawn grant did not keep the original unit".to_owned());
            }
            Ok(())
        }
        "mismatch" => {
            fixture.begin()?;
            let platform = platform();
            let mut changed = fixture.request.clone();
            changed.request_digest = Some("e".repeat(64));
            if operation::checked_operation_state(&platform, &platform, &fixture.state, &changed)
                .is_ok()
                || delete::execute_checked_delegated(
                    &fixture.state,
                    &platform,
                    &platform,
                    &platform,
                    &changed,
                    &Reaffirm,
                    DeleteBounds::default(),
                )
                .is_ok()
                || !fixture.root.join("payload").exists()
            {
                return Err("a changed original request was accepted".to_owned());
            }
            Ok(())
        }
        "intent" => {
            fixture.begin()?;
            let platform = platform();
            if delete::replay_checked_operation(
                &fixture.state,
                &platform,
                &platform,
                &platform,
                &fixture.request,
                DeleteBounds::default(),
            )
            .is_ok()
                || !fixture.root.join("payload").exists()
            {
                return Err("generic replay accepted a caller intent".to_owned());
            }
            Ok(())
        }
        "intent-capture" | "renamed-replay" | "unlink-window" | "removed" => {
            fixture.begin()?;
            match case {
                "intent-capture" => delete::fail_after_operation_capture(),
                "renamed-replay" => delete::stop_after_operation_renamed(),
                "unlink-window" => delete::fail_after_operation_unlink(),
                "removed" => delete::stop_after_operation_removed(),
                _ => unreachable!(),
            }
            let platform = platform();
            let stopped = delete::execute_checked_delegated(
                &fixture.state,
                &platform,
                &platform,
                &platform,
                &fixture.request,
                &Reaffirm,
                DeleteBounds::default(),
            )
            .map_err(|e| e.to_string())?;
            if stopped.outcome != delete::DeleteOutcome::Pending {
                return Err(format!(
                    "the injected phase did not stop the real walk: {stopped:?}"
                ));
            }
            let phase = fixture.record()?.phase;
            if case == "intent-capture" {
                if phase != OperationPhase::Intent
                    || delete::replay_checked_operation(
                        &fixture.state,
                        &platform,
                        &platform,
                        &platform,
                        &fixture.request,
                        DeleteBounds::default(),
                    )
                    .is_ok()
                {
                    return Err("generic replay accepted an interrupted caller intent".to_owned());
                }
                fixture.execute()?;
            } else if case == "renamed-replay" {
                if phase != OperationPhase::Renamed {
                    return Err("the real capture was not recorded before the stop".to_owned());
                }
                let replay = delete::replay_checked_operation(
                    &fixture.state,
                    &platform,
                    &platform,
                    &platform,
                    &fixture.request,
                    DeleteBounds::default(),
                )
                .map_err(|e| e.to_string())?;
                if !matches!(replay, delete::OperationReplay::Walked(ref report) if report.outcome == delete::DeleteOutcome::Deleted)
                    || fixture.record()?.phase != OperationPhase::Deleted
                {
                    return Err(format!("the post-capture walk did not finish: {replay:?}"));
                }
            } else if case == "removed" {
                if phase != OperationPhase::Removed || fixture.root.exists() {
                    return Err("the positive removal was not durably recorded".to_owned());
                }
                let replay = delete::replay_checked_operation(
                    &fixture.state,
                    &platform,
                    &platform,
                    &platform,
                    &fixture.request,
                    DeleteBounds::default(),
                )
                .map_err(|e| e.to_string())?;
                if !matches!(replay, delete::OperationReplay::Deleted)
                    || fixture.record()?.phase != OperationPhase::Deleted
                {
                    return Err(format!("durable removal did not settle: {replay:?}"));
                }
            } else {
                if phase == OperationPhase::Removed
                    || phase == OperationPhase::Deleted
                    || fixture.root.exists()
                {
                    return Err("the unlink-to-publication window was not preserved".to_owned());
                }
                let replay = delete::replay_checked_operation(
                    &fixture.state,
                    &platform,
                    &platform,
                    &platform,
                    &fixture.request,
                    DeleteBounds::default(),
                )
                .map_err(|e| e.to_string())?;
                if !matches!(replay, delete::OperationReplay::Pending(_)) {
                    return Err(format!("absence incorrectly proved completion: {replay:?}"));
                }
            }
            Ok(())
        }
        _ => Err(format!("case {case} is not yet implemented")),
    }
}

fn concurrent_case(fixture: &Fixture) -> Result<(), String> {
    use std::sync::{Condvar, Mutex};
    struct Rendezvous {
        arrived: Mutex<u32>,
        ready: Condvar,
    }
    impl Rendezvous {
        fn wait(&self) -> barge::error::Result<()> {
            let mut count = self.arrived.lock().map_err(|_| {
                barge::error::Error::new(
                    barge::error::ErrorKind::UnknownState,
                    "rendezvous poisoned",
                )
            })?;
            *count += 1;
            self.ready.notify_all();
            let (count, _) = self
                .ready
                .wait_timeout_while(count, std::time::Duration::from_secs(5), |count| *count < 2)
                .map_err(|_| {
                    barge::error::Error::new(
                        barge::error::ErrorKind::UnknownState,
                        "rendezvous poisoned",
                    )
                })?;
            if *count != 2 {
                return Err(barge::error::Error::new(
                    barge::error::ErrorKind::UnknownState,
                    "other dispatch did not arrive",
                ));
            }
            Ok(())
        }
    }
    struct ConcurrentHook<'a> {
        fixture: &'a Fixture,
        guard: &'a Mutex<()>,
        entered: &'a Rendezvous,
        consulted: &'a Rendezvous,
    }
    impl FinalizeHook for ConcurrentHook<'_> {
        fn kind(&self) -> &str {
            "proof.settle"
        }
        fn version(&self) -> &str {
            "1"
        }
        fn finalize(
            &self,
            context: &FinalizationContext,
            record: &OperationRecord,
        ) -> barge::error::Result<()> {
            self.entered.wait()?;
            synthetic_settle(self.fixture, self.guard, context, record)?;
            self.consulted.wait()
        }
    }
    fixture.begin()?;
    fixture.execute()?;
    let entered = Rendezvous {
        arrived: Mutex::new(0),
        ready: Condvar::new(),
    };
    let consulted = Rendezvous {
        arrived: Mutex::new(0),
        ready: Condvar::new(),
    };
    let guard = Mutex::new(());
    std::thread::scope(|scope| -> Result<(), String> {
        let invoke = || {
            let hook = ConcurrentHook {
                fixture,
                guard: &guard,
                entered: &entered,
                consulted: &consulted,
            };
            fixture.finalize(&[&hook])
        };
        let one = scope.spawn(invoke);
        let two = scope.spawn(invoke);
        for result in [one.join(), two.join()] {
            let record = result.map_err(|_| "a dispatch panicked".to_owned())??;
            if record.phase != OperationPhase::Complete {
                return Err("a dispatch failed to acknowledge".to_owned());
            }
        }
        Ok(())
    })?;
    if !fixture.settlement.exists() {
        return Err("the caller did not settle".to_owned());
    }
    Ok(())
}
