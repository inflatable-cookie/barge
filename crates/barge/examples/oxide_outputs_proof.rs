//! The Oxide outputs adapter's proof-only driver: one case per invocation.
//!
//! This executable exists for `scripts/native/oxide-outputs-proof.py`, which
//! stages it from the `proof-synthetic-account` feature build of HEAD inside
//! the guard's own scratch and runs exactly one case per process. It is not
//! product surface: no CLI verb routes here, and the example target declares
//! `required-features`, so no ordinary build compiles it.
//!
//! Each case builds its own fixture inside the directory the caller names:
//! a private HOME and `BARGE_ACCOUNT_HOME` (the proof feature binds Oxide's
//! reference directories to the synthetic home), a private `BARGE_STATE_DIR`,
//! and the outputs roots on the volume the caller attached. The volume's UUID
//! and mount arrive through `BARGE_PROOF_VOLUME_UUID` and
//! `BARGE_PROOF_VOLUME_MOUNT`; the case directory arrives through
//! `BARGE_PROOF_CASE_DIR`. Everything else — the declaration and its grant,
//! the Oxide registry and control state, the covered root and its marker, the
//! live-intent claim — is this file's fixture, byte for byte what the tests in
//! `src/oxide_outputs/tests.rs` build.
//!
//! The verdict is one JSON document on stdout:
//! `{"case": "...", "status": "passed"|"failed", "detail": "..."}`. A failed
//! case exits nonzero. Nothing here touches a real account: the production
//! account lookup is not exercised (the staged build resolves Oxide's
//! directories from the synthetic home alone), and no real drive, install or
//! adoption follows.

use std::path::PathBuf;
use std::process::ExitCode;

use barge::area::{self, OutputsGrant, PrunePolicy};
use barge::digest::sha256_hex;
use barge::error::ErrorKind;
use barge::os::{platform, DirOps, ProcessObserver, VolumeUuid};
use barge::oxide_outputs::{
    self, AbsenceLookup, AdapterBounds, AdapterCall, BargeLookup, DelegatedRequest, LiveIntent,
    Outcome, OutcomeKind, AREA_NAME, AREA_NAMESPACE, LIVE_PHASE,
};
use barge::reference::{OXIDE_INTENT_V1, OXIDE_MARKER_V2, OXIDE_REGISTRY_V2};
use barge::registry::{
    Availability, Durability, FolderOverrides, RecordOrigin, RegistryWriter, ScratchGrants,
    VolumeRecord,
};
use barge::state::{self, Deadline, IntentPhase, IntentRecord, IntentUnit, LockSet, StatePaths};
use barge::unit::{DeletionUnit, MarkerFamily, UnitGrant, UnitKind, UnitMarker};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// The caller intent id every fixture case presents, as Oxide spells one.
const CALLER_ID: &str = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
const PREVIEW: &str = "abababababababababababababababababababababababababababababababab";
const NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHECKOUT_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
/// The Barge intent id a case's own record carries.
const INTENT_ID: &str = "ddd0ddd0ddd0ddd0ddd0ddd0ddd0ddd0";
/// How long an A12 holder keeps the volume lock: strictly longer than the
/// design lock wait (state::LOCK_WAIT, 5 s), so the call's own wait refuses
/// with `Locked` instead of acquiring the lock when the holder leaves.
const HOLD_LONGER_THAN_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(7);
/// The root's name inside the outputs folder, for every case.
const ROOT_NAME: &str = "root-one";
/// The boot-volume prefix of the never-created checkout path every fixture
/// names. A mount-parent path would be refused by the reviewed F2 rule, whose
/// volume the intent identity does not carry; /private/var/tmp sits on the
/// boot Data volume, so the absence proof walks real ancestors to an ENOENT
/// leaf and answers absent.
const CHECKOUT_PATH_PREFIX: &str = "/private/var/tmp/barge-proof-checkout-";

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(case) = arguments.next() else {
        eprintln!("name the case to run");
        return ExitCode::from(ErrorKind::Usage.exit_code());
    };
    // The verdict document is printed on both paths, so the proof script
    // always parses a status from the case's own stdout: `passed` only when
    // the case's own assertions all held, `failed` with the reason otherwise.
    let verdict = match run_case(&case) {
        Ok(()) => serde_json::json!({"case": case, "status": "passed"}),
        Err(detail) => serde_json::json!({"case": case, "status": "failed", "detail": detail}),
    };
    println!("{verdict}");
    if verdict["status"] == "passed" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(ErrorKind::UnknownState.exit_code())
    }
}

/// The environment the driver requires, checked before any fixture is built.
struct Bindings {
    state: PathBuf,
    home: PathBuf,
    volume_uuid: VolumeUuid,
    volume_mount: PathBuf,
}

fn bindings() -> Result<Bindings, String> {
    let required = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{name} is not set, so this case cannot run"))
    };
    let state = required("BARGE_STATE_DIR")?;
    let home = required("HOME")?;
    let uuid_text = std::env::var("BARGE_PROOF_VOLUME_UUID")
        .map_err(|_| "BARGE_PROOF_VOLUME_UUID is not set".to_owned())?;
    let volume_mount = required("BARGE_PROOF_VOLUME_MOUNT")?;
    Ok(Bindings {
        state,
        home,
        volume_uuid: VolumeUuid::parse(&uuid_text).map_err(|error| {
            format!("BARGE_PROOF_VOLUME_UUID is not a UUID: {}", error.message())
        })?,
        volume_mount,
    })
}

fn run_case(case: &str) -> Result<(), String> {
    let bindings = bindings()?;
    let fixture = Fixture::prepare(&bindings)?;
    match case {
        "A1" => case_positive_deletion(&fixture),
        "A2" => case_withdrawn_grant_refuses(&fixture),
        "A3" => case_stale_generation_refuses(&fixture),
        "A4" => case_legacy_fence_refuses(&fixture),
        "A5" => case_unreadable_registry_is_unknown(&fixture),
        "A6" => case_settled_intent_refuses(&fixture),
        "A7" => case_changed_marker_refuses(&fixture),
        "A8" => case_generic_delete_refuses(&fixture),
        "A9" => case_live_hold_keeps(&fixture),
        "A10" => case_pre_rename_rollback(&fixture),
        "A11" => case_post_rename_resume_and_lookup(&fixture),
        "A12" => case_locked_outcomes(&fixture),
        "A13" => case_terminal_binding(&fixture),
        "A14" => case_routed_deletion(&fixture),
        "A15" => case_route_refusals(&fixture),
        "A16" => case_absence_only(&fixture),
        other => Err(format!("`{other}` is not a case this driver runs")),
    }
}

/// One case's fixture: the synthetic home, the state directory, the registered
/// volume, the declared area with its grant, and the covered root.
struct Fixture {
    state: StatePaths,
    home: PathBuf,
    outputs: PathBuf,
    root_path: PathBuf,
    marker_digest: String,
    inode: u64,
    device: u64,
    checkout: PathBuf,
    volume_uuid: VolumeUuid,
    volume_record: VolumeRecord,
    declaration: area::AreaDeclaration,
}

impl Fixture {
    fn prepare(bindings: &Bindings) -> Result<Self, String> {
        let platform = platform();
        // Under the proof feature, the platform's own AccountFacts resolve
        // Oxide's directories from the validated synthetic account home; the
        // fixture state directory is the explicit override.
        let state = StatePaths::resolve(&platform, Some(&bindings.state))
            .map_err(|error| format!("the state directory does not resolve: {error}"))?;
        state
            .initialize()
            .map_err(|error| format!("the state layout is not created: {error}"))?;
        let outputs = bindings.volume_mount.join("oxide-outputs");
        std::fs::create_dir_all(&outputs)
            .map_err(|error| format!("the outputs folder is not created: {error}"))?;
        let volume_record = VolumeRecord {
            volume_uuid: bindings.volume_uuid.clone(),
            mount_hint: Some(bindings.volume_mount.clone()),
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
        let mut locks = LockSet::with_deadline(
            state.clone(),
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        let lock = locks
            .lock_registry()
            .map_err(|error| format!("the registry lock is not taken: {error}"))?;
        RegistryWriter::new(&state)
            .publish(
                &lock,
                &barge::registry::RegistryDocument::new(vec![volume_record.clone()]),
            )
            .map_err(|error| format!("the volume is not registered: {error}"))?;
        drop(lock);
        let declaration = area::AreaDeclaration {
            namespace: AREA_NAMESPACE.to_owned(),
            area: AREA_NAME.to_owned(),
            folder: "oxide-outputs".to_owned(),
            overrides: Default::default(),
            absent: area::AbsentPolicy::Fail {
                attach: "the proof image".to_owned(),
            },
            prune: PrunePolicy::ReleaseOnReference,
            reserve: area::ReserveParticipation::Excluded,
            outputs_grant: Some(OutputsGrant::new("oxide", 1_000)),
        };
        area::declare(&state, &declaration, 1_000)
            .map_err(|error| format!("the area is not declared: {error}"))?;
        let root_path = outputs.join(ROOT_NAME);
        std::fs::create_dir_all(&root_path)
            .map_err(|error| format!("the root is not created: {error}"))?;
        std::fs::write(root_path.join("payload.txt"), b"outputs")
            .map_err(|error| format!("the payload is not written: {error}"))?;
        // The checkout is the consumer path whose absence the covered class
        // requires. It is named under the boot volume - never under a mount
        // parent, whose volume the intent identity does not carry - and it is
        // never created, so the absence proof walks real ancestors to an
        // ENOENT leaf. One name per case process.
        let checkout = PathBuf::from(format!("{CHECKOUT_PATH_PREFIX}{}", std::process::id()));
        let marker = format!(
            r#"{{"version":"{OXIDE_MARKER_V2}","checkout":"{}","checkout_id":"{CHECKOUT_ID}","label":"dev","recipe":"target-debug-v1","kind":"external","volume_uuid":"{}","nonce":"{NONCE}","generation":1,"created_ms":1000}}"#,
            checkout.display(),
            bindings.volume_uuid,
        );
        let marker_digest = sha256_hex(marker.as_bytes());
        std::fs::write(root_path.join(barge::reference::OXIDE_MARKER_FILE), &marker)
            .map_err(|error| format!("the marker is not written: {error}"))?;
        let device = platform
            .open_path(&bindings.volume_mount)
            .map_err(|error| format!("the volume mount is not opened: {error}"))?
            .identity()
            .device;
        let inode = platform
            .open_path(&root_path)
            .map_err(|error| format!("the root is not opened: {error}"))?
            .identity()
            .inode;
        Ok(Self {
            state,
            home: bindings.home.clone(),
            outputs,
            root_path,
            marker_digest,
            inode,
            device: device.get(),
            checkout,
            volume_uuid: bindings.volume_uuid.clone(),
            volume_record,
            declaration,
        })
    }

    fn live_intent(&self, caller_id: &str) -> LiveIntent {
        LiveIntent {
            id: caller_id.to_owned(),
            phase: LIVE_PHASE.to_owned(),
            preview_sha256: PREVIEW.to_owned(),
            identity: barge::reference::OxideIntentIdentity {
                checkout_id: CHECKOUT_ID.to_owned(),
                checkout: self.checkout.clone(),
                receipt: self
                    .checkout
                    .parent()
                    .expect("the home")
                    .join("receipt.json"),
                member: "target".to_owned(),
                root: self.root_path.clone(),
                root_kind: "external".to_owned(),
                volume_uuid: Some(self.volume_uuid.as_str().to_owned()),
                nonce: Some(NONCE.to_owned()),
                generation: 1,
                dev: self.device,
                ino: self.inode,
                marker_digest: self.marker_digest.clone(),
            },
        }
    }

    fn request() -> DelegatedRequest {
        DelegatedRequest {
            grant_generation: 1,
            request_sha256: None,
        }
    }

    /// Oxide's control directory inside the synthetic home: the same relative
    /// layout the proof build's AccountFacts resolve.
    fn control(&self) -> PathBuf {
        self.home
            .join("Library/Caches/oxide")
            .join("build-pool")
            .join("oxide-managed")
    }

    fn write_control_state(&self, fence: Option<&str>, version: u32) -> Result<(), String> {
        let mut document = serde_json::json!({
            "version": version,
            "next_id": 1,
            "next_use": 1,
            "tickets": [],
            "records": [],
        });
        if let Some(fence) = fence {
            document["owned_outputs"] = serde_json::json!(fence);
        }
        std::fs::create_dir_all(self.control())
            .map_err(|error| format!("the control directory is not created: {error}"))?;
        std::fs::write(
            self.control().join("state.json"),
            serde_json::to_vec_pretty(&document).expect("the control state encodes"),
        )
        .map_err(|error| format!("the control state is not written: {error}"))
    }

    fn intent_value(&self, caller_id: &str, phase: &str) -> serde_json::Value {
        serde_json::json!({
            "version": OXIDE_INTENT_V1,
            "id": caller_id,
            "identity": {
                "checkout_id": CHECKOUT_ID,
                "checkout": self.checkout,
                "receipt": self.checkout.parent().expect("the home").join("receipt.json"),
                "member": "target",
                "root": self.root_path,
                "root_kind": "external",
                "volume_uuid": self.volume_uuid.as_str(),
                "nonce": NONCE,
                "generation": 1,
                "dev": self.device,
                "ino": self.inode,
                "marker_digest": self.marker_digest,
            },
            "phase": phase,
            "quarantine": self.outputs.join(format!(".prune-{caller_id}")),
            "preview_sha256": PREVIEW,
            "created_ms": 1_000,
            "updated_ms": 2_000,
        })
    }

    fn write_registry(&self, intents: Vec<serde_json::Value>) -> Result<(), String> {
        let outputs = self.control().join("outputs");
        std::fs::create_dir_all(&outputs)
            .map_err(|error| format!("Oxide's outputs folder is not created: {error}"))?;
        let row = serde_json::json!({
            "checkout_id": CHECKOUT_ID,
            "checkout": self.checkout,
            "receipt": self.checkout.parent().expect("the home").join("receipt.json"),
            "member": "target",
            "root": self.root_path,
            "root_kind": "external",
            "recipe": "target-debug-v1",
            "generation": 1,
            "selected": false,
            "pending": false,
        });
        let document = serde_json::json!({
            "version": OXIDE_REGISTRY_V2,
            "schema": 2,
            "revision": 1,
            "complete": true,
            "created_by": "oxide",
            "rows": [row],
            "intents": intents,
        });
        std::fs::write(
            outputs.join("registry.json"),
            serde_json::to_vec_pretty(&document).expect("the registry encodes"),
        )
        .map_err(|error| format!("the registry is not written: {error}"))
    }

    /// The covered class, complete: successor fence, ready registry, live intent.
    fn covered(&self, caller_id: &str) -> Result<(), String> {
        self.write_control_state(Some("oxide.outputs/3"), 4)?;
        self.write_registry(vec![self.intent_value(caller_id, "intent")])
    }

    /// The unit this fixture's root is, as the journal records it.
    fn unit(&self) -> DeletionUnit {
        DeletionUnit {
            volume_uuid: self.volume_uuid.clone(),
            root_dir: String::new(),
            folder: "oxide-outputs".to_owned(),
            parent: String::new(),
            name: ROOT_NAME.to_owned(),
            root_id: None,
            kind: UnitKind::Directory,
            device: barge::os::DeviceId::new(self.device),
            inode: self.inode,
            marker: Some(UnitMarker::new(
                MarkerFamily::OxideOutputs,
                self.marker_digest.clone(),
            )),
            grant: UnitGrant::delegation(MarkerFamily::OxideOutputs),
            plan_digest: "0".repeat(64),
        }
    }

    /// One adapter call over this fixture, with the platform facts.
    fn call(&self, intent: &LiveIntent, request: &DelegatedRequest) -> Result<Outcome, String> {
        let platform = platform();
        let call = AdapterCall {
            state_paths: &self.state,
            dirs: &platform,
            facts: &platform,
            mounts: &platform,
            observer: &platform,
            account: &platform,
            intent,
            request,
            bounds: AdapterBounds::design(),
        };
        Ok(oxide_outputs::execute(&call))
    }

    fn lookup(&self, caller_id: &str, binding: Option<&str>) -> Result<BargeLookup, String> {
        let platform = platform();
        Ok(oxide_outputs::lookup(
            &self.state,
            &platform,
            &platform,
            &platform,
            &platform,
            &self.live_intent(caller_id),
            &Self::request(),
            binding,
        ))
    }

    /// The one record the adapter or a fixture published for `intent_id`.
    fn record(&self, intent_id: &str) -> Result<IntentRecord, String> {
        state::read_intent(&self.state, intent_id)
            .map_err(|error| format!("the record {intent_id} does not read back: {error}"))
    }

    /// Publish one pending record exactly as the adapter's own writer would.
    fn publish_record(
        &self,
        intent_id: &str,
        caller_id: &str,
        phase: IntentPhase,
    ) -> Result<IntentRecord, String> {
        let request_digest = Fixture::request()
            .digest(&self.live_intent(caller_id))
            .map_err(|error| format!("the request does not digest: {error}"))?;
        let now = state::now_ms();
        let intent = IntentRecord {
            schema: barge::contract::INTENT_SCHEMA.to_owned(),
            intent_id: intent_id.to_owned(),
            volume_uuid: self.volume_uuid.clone(),
            mount_hint: self.volume_record.mount_hint.clone().unwrap_or_default(),
            root_dir: String::new(),
            folder: "oxide-outputs".to_owned(),
            unit: IntentUnit {
                parent: String::new(),
                name: ROOT_NAME.to_owned(),
                kind: UnitKind::Directory,
                device: self.device,
                inode: self.inode,
                marker: Some(UnitMarker::new(
                    MarkerFamily::OxideOutputs,
                    self.marker_digest.clone(),
                )),
                root_id: None,
                capture_name: format!("{}{intent_id}", state::QUARANTINE_PREFIX),
                grant: UnitGrant::delegation(MarkerFamily::OxideOutputs),
            },
            plan_digest: request_digest,
            phase: IntentPhase::Intent,
            generation: 1,
            recorded_ms: now,
            phase_ms: now,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
            identity: None,
            absence: None,
            caller_intent_id: Some(caller_id.to_owned()),
            caller_request_digest: Some(
                Fixture::request()
                    .digest(&self.live_intent(caller_id))
                    .map_err(|error| format!("the request does not digest: {error}"))?,
            ),
        };
        let mut locks = LockSet::new(self.state.clone());
        let _volume = locks
            .lock_volume(&self.volume_uuid)
            .map_err(|error| format!("the volume lock is not taken: {error}"))?;
        let _unit = locks
            .lock_unit(&state::unit_lock_key(&self.volume_uuid, "", ROOT_NAME))
            .map_err(|error| format!("the unit lock is not taken: {error}"))?;
        let write = locks
            .begin_intent_write()
            .map_err(|error| format!("the intent step is not entered: {error}"))?;
        let mut published = intent.clone();
        state::publish_intent(&write, &self.state, &published)
            .map_err(|error| format!("the intent is not published: {error}"))?;
        if phase != IntentPhase::Intent {
            published = state::advance_intent(&write, &self.state, &published, phase)
                .map_err(|error| format!("the phase does not advance: {error}"))?;
        }
        Ok(published)
    }

    fn declare_without_grant(&self) -> Result<(), String> {
        let mut declaration = self.declaration.clone();
        declaration.outputs_grant = None;
        area::declare(&self.state, &declaration, 2_000)
            .map(|_| ())
            .map_err(|error| format!("the withdrawal is not declared: {error}"))
    }
}

// The cases. Each returns Ok(()) on pass and Err(reason) on fail; the driver
// prints the verdict document either way.

fn case_positive_deletion(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let intent = fixture.live_intent(CALLER_ID);
    let outcome = fixture.call(&intent, &Fixture::request())?;
    if outcome.kind != OutcomeKind::Deleted {
        return Err(format!("expected deleted, got {outcome:?}"));
    }
    let intent_id = outcome
        .intent_id
        .clone()
        .ok_or_else(|| "the deleted outcome names no binding".to_owned())?;
    if fixture.root_path.exists() {
        return Err("the root still exists".to_owned());
    }
    let record = fixture.record(&intent_id)?;
    if record.phase != IntentPhase::Deleted || record.caller_intent_id.as_deref() != Some(CALLER_ID)
    {
        return Err(format!(
            "the settled record is not the caller's: {record:?}"
        ));
    }
    if fixture
        .outputs
        .join(record.unit.capture_name.clone())
        .exists()
    {
        return Err("the quarantine leaf outlived the settled intent".to_owned());
    }
    Ok(())
}

fn case_withdrawn_grant_refuses(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    fixture.declare_without_grant()?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Refused {
        return Err(format!("expected refused, got {outcome:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("a withdrawn grant deleted the root".to_owned());
    }
    Ok(())
}

fn case_stale_generation_refuses(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let request = DelegatedRequest {
        request_sha256: None,
        grant_generation: 9,
    };
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &request)?;
    if outcome.kind != OutcomeKind::Refused {
        return Err(format!("expected refused, got {outcome:?}"));
    }
    let reason = outcome.reason.unwrap_or_default();
    if !reason.contains("generation") {
        return Err(format!(
            "the refusal does not name the generation: {reason}"
        ));
    }
    if !fixture.root_path.exists() {
        return Err("a stale generation deleted the root".to_owned());
    }
    Ok(())
}

fn case_legacy_fence_refuses(fixture: &Fixture) -> Result<(), String> {
    fixture.write_control_state(Some("oxide.outputs/2"), 3)?;
    fixture.write_registry(vec![fixture.intent_value(CALLER_ID, "intent")])?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Refused {
        return Err(format!("expected refused, got {outcome:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("a legacy fence deleted the root".to_owned());
    }
    Ok(())
}

fn case_unreadable_registry_is_unknown(fixture: &Fixture) -> Result<(), String> {
    fixture.write_control_state(Some("oxide.outputs/3"), 4)?;
    let outputs = fixture.control().join("outputs");
    std::fs::create_dir_all(&outputs)
        .map_err(|error| format!("Oxide's outputs folder is not created: {error}"))?;
    std::fs::write(outputs.join("registry.json"), b"{ not json")
        .map_err(|error| format!("the garbage registry is not written: {error}"))?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Unknown {
        return Err(format!("expected unknown, got {outcome:?}"));
    }
    // The Unknown must be this case's own fact - an unreadable registry - and
    // not any unrelated unknown: a control directory that cannot even be
    // resolved must not read as this case's pass.
    let reason = outcome.reason.clone().unwrap_or_default();
    if !reason.contains("registry") {
        return Err(format!(
            "the Unknown does not name the unreadable registry, so an unrelated unknown cannot be told from this case's: {reason}"
        ));
    }
    if !fixture.root_path.exists() {
        return Err("an unknown authority deleted the root".to_owned());
    }
    Ok(())
}

fn case_settled_intent_refuses(fixture: &Fixture) -> Result<(), String> {
    fixture.write_control_state(Some("oxide.outputs/3"), 4)?;
    fixture.write_registry(vec![fixture.intent_value(CALLER_ID, "deleted")])?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Refused {
        return Err(format!("expected refused, got {outcome:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("a settled intent deleted the root".to_owned());
    }
    Ok(())
}

fn case_changed_marker_refuses(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let marker = format!(
        r#"{{"version":"{OXIDE_MARKER_V2}","checkout":"{}","checkout_id":"{CHECKOUT_ID}","label":"dev","recipe":"target-debug-v1","kind":"external","volume_uuid":"{}","nonce":"{NONCE}","generation":2,"created_ms":1000}}"#,
        fixture.checkout.display(),
        fixture.volume_uuid,
    );
    std::fs::write(
        fixture.root_path.join(barge::reference::OXIDE_MARKER_FILE),
        marker,
    )
    .map_err(|error| format!("the marker is not rewritten: {error}"))?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Refused {
        return Err(format!("expected refused, got {outcome:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("other marker bytes did not keep the root".to_owned());
    }
    Ok(())
}

fn case_generic_delete_refuses(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    fixture.publish_record(INTENT_ID, CALLER_ID, IntentPhase::Intent)?;
    let platform = platform();
    let error = barge::delete::delete_unit_on_volume(
        &fixture.state,
        &platform,
        &platform,
        &platform,
        &platform,
        &fixture.declaration,
        &fixture.unit(),
        barge::delete::DeleteBounds::default(),
    )
    .err()
    .ok_or_else(|| "a generic delete of the family was not refused".to_owned())?;
    if error.kind() != ErrorKind::RefusedBoundary {
        return Err(format!("expected RefusedBoundary, got {error:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("the generic path deleted the root".to_owned());
    }
    let replay = barge::delete::replay_pending(
        &fixture.state,
        &platform,
        &platform,
        &platform,
        barge::delete::DeleteBounds::default(),
        None,
    )
    .map_err(|error| format!("the replay failed: {error}"))?;
    if replay.pending.len() != 1 {
        return Err(format!(
            "the generic replay did not keep the intent: {replay:?}"
        ));
    }
    if fixture.record(INTENT_ID)?.phase != IntentPhase::Intent {
        return Err("the generic replay changed the phase".to_owned());
    }
    Ok(())
}

fn case_live_hold_keeps(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let hold_dir = fixture.state.holds_dir().join(ROOT_NAME);
    std::fs::create_dir_all(&hold_dir)
        .map_err(|error| format!("the hold directory is not created: {error}"))?;
    let holder = std::process::id();
    let start = platform()
        .process_start(holder)
        .map_err(|error| format!("this process's start identity is unreadable: {error}"))?
        .ok_or_else(|| "this process reports no start identity".to_owned())?;
    let boot = platform()
        .boot_identity()
        .map_err(|error| format!("the boot identity is unreadable: {error}"))?;
    let hold = serde_json::json!({
        "schema": barge::contract::HOLD_SCHEMA,
        "root_id": ROOT_NAME,
        "holder": "proof-holder",
        "pid": holder,
        "process_start": start,
        "boot_identity": boot,
        "scope": "write",
        "purpose": "the proof's own live hold",
        "held_ms": state::now_ms(),
    });
    // The hold record is private state: custody refuses a hold file whose
    // mode is not owner-only, so the fixture must create it 0600 for the
    // held branch to be decidable at all.
    let hold_path = hold_dir.join("proof-holder.json");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&hold_path)
            .map_err(|error| format!("the hold is not created: {error}"))?;
        file.write_all(&serde_json::to_vec_pretty(&hold).expect("the hold encodes"))
            .map_err(|error| format!("the hold is not written: {error}"))?;
    }
    // The decidable-live premise is observed, not assumed: the hold file on
    // disk must be owner-only before the call, or custody cannot decide it.
    let hold_mode = std::fs::metadata(&hold_path)
        .map_err(|error| format!("the hold's mode is unreadable: {error}"))?
        .permissions()
        .mode();
    if hold_mode & 0o077 != 0 {
        return Err(format!(
            "the fixture hold is mode {hold_mode:o}, not owner-only, so custody cannot decide it and this case's premise failed"
        ));
    }
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Refused {
        return Err(format!("expected refused, got {outcome:?}"));
    }
    // The refusal must be custody's own fact - the live hold - and not an
    // earlier refusal such as the checkout absence proof, which runs before
    // custody in the delete path. Custody names the holder as live.
    let reason = outcome.reason.clone().unwrap_or_default();
    if !reason.contains("custody is not proven") || !reason.contains("is live") {
        return Err(format!(
            "the refusal is not custody's live-hold fact, so an unrelated refusal cannot be told from this case's: {reason}"
        ));
    }
    if !fixture.root_path.exists() {
        return Err("a held root was deleted".to_owned());
    }
    Ok(())
}

fn case_pre_rename_rollback(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    fixture.publish_record(INTENT_ID, CALLER_ID, IntentPhase::Intent)?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::RolledBack {
        return Err(format!("expected rolled_back, got {outcome:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("the rolled-back root did not stay".to_owned());
    }
    if fixture.record(INTENT_ID).is_ok() {
        return Err("the rolled-back record was not removed".to_owned());
    }
    match fixture.lookup(CALLER_ID, Some(INTENT_ID))? {
        BargeLookup::PositivelyAbsent => Ok(()),
        answer => Err(format!("expected positively absent, got {answer:?}")),
    }
}

fn case_post_rename_resume_and_lookup(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let record = fixture.publish_record(INTENT_ID, CALLER_ID, IntentPhase::Renamed)?;
    let quarantine = fixture.outputs.join(record.unit.capture_name.clone());
    std::fs::rename(&fixture.root_path, &quarantine)
        .map_err(|error| format!("the capture rename did not happen: {error}"))?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &Fixture::request())?;
    if outcome.kind != OutcomeKind::Deleted {
        return Err(format!("expected deleted from the resume, got {outcome:?}"));
    }
    if quarantine.exists() {
        return Err("the resumed deletion left its quarantine".to_owned());
    }
    match fixture.lookup(CALLER_ID, Some(INTENT_ID))? {
        BargeLookup::FoundTerminal { intent_id } if intent_id == INTENT_ID => Ok(()),
        answer => Err(format!("expected found-terminal, got {answer:?}")),
    }
}

fn case_locked_outcomes(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let intent = fixture.live_intent(CALLER_ID);
    // Another holder keeps the volume's exclusive lock; this call's own
    // `Locked` return is then the known no-effect fact.
    let state = fixture.state.clone();
    let uuid = fixture.volume_uuid.clone();
    // The holder must outlast the design lock wait (state::LOCK_WAIT, 5 s):
    // a shorter hold would let the call acquire the lock and proceed, which
    // is exactly how this path stayed unexercised in earlier occurrences.
    // Readiness is observed, not assumed: the holder acknowledges only
    // after its lock is positively held, and the case fails its own
    // premise rather than invoking the adapter against a lock nobody
    // holds.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || -> Result<(), barge::error::Error> {
        let mut locks =
            LockSet::with_deadline(state, Deadline::after(std::time::Duration::from_secs(30)));
        let _lock = locks.lock_volume(&uuid)?;
        // Acknowledge only after the lock is positively held.
        let _ = ready_tx.send(());
        std::thread::sleep(HOLD_LONGER_THAN_LOCK_WAIT);
        Ok(())
    });
    let ready = wait_for_lock_ready(&ready_rx);
    let outcome = match ready {
        Ok(()) => fixture.call(&intent, &Fixture::request()),
        Err(reason) => Err(reason),
    };
    // The holder is joined on every path, whatever the adapter answered.
    finish_holder(holder)?;
    let outcome = outcome?;
    if outcome.kind != OutcomeKind::KnownNoEffectLocked {
        return Err(format!("expected known_no_effect_locked, got {outcome:?}"));
    }
    if !fixture.root_path.exists() {
        return Err("the locked call deleted the root".to_owned());
    }

    // With a pending intent for this caller id, a locked call stays pending.
    fixture.publish_record(INTENT_ID, CALLER_ID, IntentPhase::Intent)?;
    let state = fixture.state.clone();
    let uuid = fixture.volume_uuid.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || -> Result<(), barge::error::Error> {
        let mut locks =
            LockSet::with_deadline(state, Deadline::after(std::time::Duration::from_secs(30)));
        let _lock = locks.lock_volume(&uuid)?;
        // Acknowledge only after the lock is positively held.
        let _ = ready_tx.send(());
        std::thread::sleep(HOLD_LONGER_THAN_LOCK_WAIT);
        Ok(())
    });
    let ready = wait_for_lock_ready(&ready_rx);
    let outcome = match ready {
        Ok(()) => fixture.call(&intent, &Fixture::request()),
        Err(reason) => Err(reason),
    };
    // The holder is joined on every path, whatever the adapter answered.
    finish_holder(holder)?;
    let outcome = outcome?;
    if outcome.kind != OutcomeKind::Pending || outcome.intent_id.as_deref() != Some(INTENT_ID) {
        return Err(format!(
            "expected pending with the binding, got {outcome:?}"
        ));
    }
    Ok(())
}

/// Join one holder thread and require its lock work to have succeeded.
fn finish_holder(
    holder: std::thread::JoinHandle<Result<(), barge::error::Error>>,
) -> Result<(), String> {
    holder
        .join()
        .map_err(|error| format!("the holder panicked: {error:?}"))?
        .map_err(|error| format!("the holder failed: {}", error.message()))
}

/// How long the case waits for the holder's lock-acquired acknowledgement.
/// Bounded within the case envelope: a holder that has not acquired inside
/// this window is a failed premise, never an unbounded wait.
const LOCK_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait for the holder's acknowledgement, which it sends only after its
/// volume lock is positively held. A timeout or a dead holder is a failed
/// premise: the case never invokes the adapter without observed readiness.
fn wait_for_lock_ready(ready: &std::sync::mpsc::Receiver<()>) -> Result<(), String> {
    ready.recv_timeout(LOCK_READY_TIMEOUT).map_err(|error| {
        format!(
            "the holder's lock-acquired acknowledgement did not arrive inside {LOCK_READY_TIMEOUT:?}, so the held-lock premise failed: {error}"
        )
    })
}

/// Actual native deletion followed by mismatched terminal recovery requests.
fn case_terminal_binding(fixture: &Fixture) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    let intent = fixture.live_intent(CALLER_ID);
    let request = Fixture::request();
    let deleted = fixture.call(&intent, &request)?;
    if deleted.kind != OutcomeKind::Deleted {
        return Err(format!("binding setup expected deleted, got {deleted:?}"));
    }
    let id = deleted.intent_id.ok_or("deleted report has no Barge id")?;
    let path = state::intent_path(&fixture.state, &id).map_err(|error| error.to_string())?;
    let before = std::fs::read(&path).map_err(|error| error.to_string())?;
    let mut wrong = intent.clone();
    wrong.preview_sha256 = "f".repeat(64);
    let outcome = fixture.call(&wrong, &request)?;
    if outcome.kind != OutcomeKind::Unknown
        || !outcome.reason.as_deref().unwrap_or("").contains("binding")
    {
        return Err(format!(
            "wrong terminal request must stay unknown for binding: {outcome:?}"
        ));
    }
    let platform = platform();
    let answer = oxide_outputs::lookup(
        &fixture.state,
        &platform,
        &platform,
        &platform,
        &platform,
        &wrong,
        &request,
        Some(&id),
    );
    if !matches!(answer, BargeLookup::Unknown { ref reason } if reason.contains("binding")) {
        return Err(format!(
            "wrong terminal lookup must stay unknown for binding: {answer:?}"
        ));
    }
    let other_id = if id == INTENT_ID {
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    } else {
        INTENT_ID
    };
    let answer = fixture.lookup(CALLER_ID, Some(other_id))?;
    if !matches!(answer, BargeLookup::Unknown { ref reason } if reason.contains("binding")) {
        return Err(format!(
            "wrong Barge id must stay unknown for binding: {answer:?}"
        ));
    }
    if !matches!(fixture.lookup(CALLER_ID, Some(&id))?, BargeLookup::FoundTerminal { intent_id } if intent_id == id)
    {
        return Err("the exact terminal lookup did not recover its own record".to_owned());
    }
    if fixture.call(&intent, &request)?.kind != OutcomeKind::Deleted {
        return Err("the exact repeated request did not return deleted".to_owned());
    }
    if std::fs::read(&path).map_err(|error| error.to_string())? != before {
        return Err("binding checks rewrote the terminal record".to_owned());
    }
    Ok(())
}

/// Schema 3 binds sidecar bytes separately from the canonical request digest.
fn routed_request() -> DelegatedRequest {
    DelegatedRequest {
        grant_generation: 1,
        request_sha256: Some("c".repeat(64)),
    }
}

/// Publish the successor representation in this private fixture only.
fn write_routes(fixture: &Fixture, routes: serde_json::Value) -> Result<(), String> {
    fixture.covered(CALLER_ID)?;
    fixture.write_control_state(Some("oxide.outputs/4"), 4)?;
    let path = fixture.control().join("outputs/registry.json");
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    let mut document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    document["version"] = serde_json::json!("oxide.outputs.registry/3");
    document["schema"] = serde_json::json!(3);
    document["deletion_routes"] = routes;
    std::fs::write(
        &path,
        serde_json::to_vec(&document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn case_routed_deletion(fixture: &Fixture) -> Result<(), String> {
    let request = routed_request();
    write_routes(
        fixture,
        serde_json::json!([{
            "intent_id": CALLER_ID, "deleter": "barge", "request_sha256": request.request_sha256
        }]),
    )?;
    let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &request)?;
    if outcome.kind != OutcomeKind::Deleted || fixture.root_path.exists() {
        return Err(format!(
            "the exact schema3 Barge route did not delete: {outcome:?}"
        ));
    }
    let id = outcome
        .intent_id
        .ok_or("the routed deletion has no Barge id")?;
    if fixture.record(&id)?.phase != IntentPhase::Deleted {
        return Err("the routed deletion did not settle its own record".to_owned());
    }
    Ok(())
}

fn case_route_refusals(fixture: &Fixture) -> Result<(), String> {
    let request = routed_request();
    let marker = std::fs::read(fixture.root_path.join(barge::reference::OXIDE_MARKER_FILE))
        .map_err(|error| error.to_string())?;
    for routes in [
        serde_json::json!([{"intent_id": CALLER_ID, "deleter": "local"}]),
        serde_json::json!([]),
        serde_json::json!([{"intent_id": CALLER_ID, "deleter": "barge", "request_sha256": "bad"}]),
        serde_json::json!([{"intent_id": CALLER_ID, "deleter": "barge", "request_sha256": "d".repeat(64)}]),
    ] {
        write_routes(fixture, routes)?;
        let outcome = fixture.call(&fixture.live_intent(CALLER_ID), &request)?;
        if !matches!(outcome.kind, OutcomeKind::Refused | OutcomeKind::Unknown) {
            return Err(format!("a prohibited route did not refuse: {outcome:?}"));
        }
        if !fixture.root_path.join("payload.txt").exists()
            || std::fs::read(fixture.root_path.join(barge::reference::OXIDE_MARKER_FILE))
                .map_err(|error| error.to_string())?
                != marker
        {
            return Err("a route refusal changed the root".to_owned());
        }
        let journal = state::read_intents(&fixture.state, barge::contract::INTENT_ROWS)
            .map_err(|error| error.to_string())?;
        if journal.truncated || !journal.records.is_empty() {
            return Err("a route refusal wrote a journal record".to_owned());
        }
    }
    Ok(())
}

fn case_absence_only(fixture: &Fixture) -> Result<(), String> {
    let platform = platform();
    let intent = fixture.live_intent(CALLER_ID);
    let query = || {
        oxide_outputs::lookup_absence(
            &fixture.state,
            &platform,
            &platform,
            &platform,
            &platform,
            &intent,
        )
    };
    if !matches!(query(), AbsenceLookup::PositivelyAbsent) {
        return Err("an empty complete journal and clear folder did not prove absence".to_owned());
    }
    let record = fixture.publish_record(INTENT_ID, CALLER_ID, IntentPhase::Intent)?;
    let path =
        state::intent_path(&fixture.state, &record.intent_id).map_err(|error| error.to_string())?;
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    if !matches!(query(), AbsenceLookup::Unknown { ref reason } if reason.contains("pending or terminal record"))
    {
        return Err("absence-only did not refuse the matching pending record".to_owned());
    }
    if std::fs::read(&path).map_err(|error| error.to_string())? != bytes
        || !fixture.root_path.join("payload.txt").exists()
    {
        return Err("absence-only changed the journal or root".to_owned());
    }
    Ok(())
}
