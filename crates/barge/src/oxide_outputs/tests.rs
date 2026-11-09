//! The adapter's regressions: each test discriminates one claimed fact of the
//! delegated deletion — the authority it reads, the identity it binds, the
//! outcome vocabulary it reports, and the recovery behavior it resumes — over
//! real fixture files through the real OS layer, with the caller's records
//! built exactly as Oxide writes them.

use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use super::{
    execute, lookup, AdapterBounds, AdapterCall, BargeLookup, DelegatedRequest, LiveIntent,
    OutcomeKind, AREA_NAME, AREA_NAMESPACE, LIVE_PHASE,
};
use crate::area::{self, OutputsGrant, PrunePolicy};
use crate::contract::INTENT_SCHEMA;
use crate::digest::sha256_hex;
use crate::os::{platform, VolumeUuid};
use crate::reference::{OXIDE_INTENT_V1, OXIDE_MARKER_V2, OXIDE_REGISTRY_V2};
use crate::state::{self, Deadline, IntentPhase, IntentRecord, IntentUnit, LockSet};
use crate::testkit::{FakeAccount, FakeVolume, VolumeFixture, FIXTURE_UUID};
use crate::unit::{DeletionUnit, MarkerFamily, UnitGrant, UnitKind, UnitMarker};

/// One caller intent id, exactly as Oxide's own 128-bit nonces are spelled.
const CALLER_ID: &str = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
const OTHER_CALLER_ID: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";
/// The preview digest every fixture intent carries.
const PREVIEW: &str = "abababababababababababababababababababababababababababababababab";
/// The root nonce every fixture marker carries.
const NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// The checkout id the fixture markers and registry rows carry.
const CHECKOUT_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const INTENT_ID: &str = "ddd0ddd0ddd0ddd0ddd0ddd0ddd0ddd0";

/// One fixture: a registered expendable volume, the declared `oxide/outputs`
/// area with its grant, an account home whose Oxide records are the caller's,
/// and one Oxide-marked root in the outputs folder.
struct Fixture {
    volume: VolumeFixture,
    account: FakeAccount,
    /// The outputs folder on the fixture volume.
    outputs: PathBuf,
    /// The Oxide control directory inside the account home.
    control: PathBuf,
    /// The checkout the markers name (never created).
    checkout: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let volume = VolumeFixture::new(label);
        volume.register();
        let home = volume.dir.path().join("home");
        std::fs::create_dir_all(&home).expect("the fixture home is created");
        let account = FakeAccount { home: home.clone() };
        let outputs = volume.root.join("oxide-outputs");
        std::fs::create_dir_all(&outputs).expect("the outputs folder is created");
        let control = home
            .join("Library/Caches/oxide")
            .join(crate::inspect::OXIDE_DEFAULT_POOL_DIR)
            .join(crate::inspect::OXIDE_CONTROL_DIR);
        let declaration = granted_declaration();
        area::declare(&volume.state, &declaration, 1_000).expect("the area is declared");
        Self {
            volume,
            account,
            outputs,
            control,
            checkout: home.join("checkout"),
        }
    }

    /// One Oxide root in the outputs folder, with its `/2` marker bytes, and
    /// the registry intent that claims it live.
    fn root(&self, name: &str) -> RootFixture {
        let path = self.outputs.join(name);
        std::fs::create_dir_all(&path).expect("the root is created");
        std::fs::write(path.join("payload.txt"), b"outputs").expect("the payload is written");
        let marker = format!(
            r#"{{"version":"{OXIDE_MARKER_V2}","checkout":"{}","checkout_id":"{CHECKOUT_ID}","label":"dev","recipe":"target-debug-v1","kind":"external","volume_uuid":"{FIXTURE_UUID}","nonce":"{NONCE}","generation":1,"created_ms":1000}}"#,
            self.checkout.display(),
        );
        let marker_digest = sha256_hex(marker.as_bytes());
        std::fs::write(path.join(crate::reference::OXIDE_MARKER_FILE), &marker)
            .expect("the marker is written");
        let metadata = std::fs::metadata(&path).expect("the root is statable");
        RootFixture {
            path,
            nonce: NONCE.to_owned(),
            marker_digest,
            inode: metadata.ino(),
            device: self.volume.device.get(),
            preview: PREVIEW.to_owned(),
        }
    }

    /// The caller's claim about this root, as its prune intent names it.
    fn live_intent(&self, root: &RootFixture, caller_id: &str) -> LiveIntent {
        LiveIntent {
            id: caller_id.to_owned(),
            phase: LIVE_PHASE.to_owned(),
            preview_sha256: root.preview.clone(),
            identity: crate::reference::OxideIntentIdentity {
                checkout_id: CHECKOUT_ID.to_owned(),
                checkout: self.checkout.clone(),
                receipt: self.account.home.join("receipt.json"),
                member: "target".to_owned(),
                root: root.path.clone(),
                root_kind: "external".to_owned(),
                volume_uuid: Some(FIXTURE_UUID.to_owned()),
                nonce: Some(root.nonce.clone()),
                generation: 1,
                dev: root.device,
                ino: root.inode,
                marker_digest: root.marker_digest.clone(),
            },
        }
    }

    /// One adapter call over this fixture.
    fn mounts_of(&self) -> crate::testkit::FakeMounts {
        self.volume.mounts()
    }

    /// The registry document with one settled row and these intents.
    fn write_registry(&self, root: &RootFixture, intents: Vec<serde_json::Value>) {
        let outputs = self.control.join(crate::reference::OXIDE_OUTPUTS_DIR);
        std::fs::create_dir_all(&outputs).expect("Oxide's outputs folder is created");
        let row = json!({
            "checkout_id": CHECKOUT_ID,
            "checkout": self.checkout,
            "receipt": self.account.home.join("receipt.json"),
            "member": "target",
            "root": root.path,
            "root_kind": "external",
            "recipe": "target-debug-v1",
            "generation": 1,
            "selected": false,
            "pending": false,
        });
        let document = json!({
            "version": OXIDE_REGISTRY_V2,
            "schema": 2,
            "revision": 1,
            "complete": true,
            "created_by": "oxide",
            "rows": [row],
            "intents": intents,
        });
        let mut bytes = serde_json::to_vec_pretty(&document).expect("the registry encodes");
        bytes.push(b'\n');
        std::fs::write(outputs.join(crate::reference::OXIDE_REGISTRY_FILE), bytes)
            .expect("the registry is written");
    }

    /// Oxide's control state at `version`, carrying `fence` (when it does).
    fn write_control_state(&self, fence: Option<&str>, version: u32) {
        let mut document = json!({
            "version": version,
            "next_id": 1,
            "next_use": 1,
            "tickets": [],
            "records": [],
        });
        if let Some(fence) = fence {
            document["owned_outputs"] = json!(fence);
        }
        std::fs::create_dir_all(&self.control).expect("the control directory is created");
        std::fs::write(
            self.control.join("state.json"),
            serde_json::to_vec_pretty(&document).expect("the control state encodes"),
        )
        .expect("the control state is written");
    }

    /// The covered class, complete: successor fence, ready registry, and the
    /// one live intent for `root` under `caller_id`.
    fn covered(&self, root: &RootFixture, caller_id: &str) {
        self.write_control_state(Some("oxide.outputs/3"), 4);
        self.write_registry(root, vec![self.intent_value(root, caller_id, "intent")]);
    }

    /// The area declaration this fixture stores, with its grant.
    fn granted_declaration() -> area::AreaDeclaration {
        granted_declaration()
    }

    /// Declare the area without the grant (the withdrawal form).
    fn withdraw_grant(&self) {
        let mut declaration = granted_declaration();
        declaration.outputs_grant = None;
        area::declare(&self.volume.state, &declaration, 2_000).expect("the withdrawal is declared");
    }
}

/// The `oxide/outputs` declaration every fixture stores.
fn granted_declaration() -> area::AreaDeclaration {
    area::AreaDeclaration {
        namespace: AREA_NAMESPACE.to_owned(),
        area: AREA_NAME.to_owned(),
        folder: "oxide-outputs".to_owned(),
        overrides: Default::default(),
        absent: area::AbsentPolicy::Fail {
            attach: "the build volume".to_owned(),
        },
        prune: PrunePolicy::ReleaseOnReference,
        reserve: area::ReserveParticipation::Excluded,
        outputs_grant: Some(OutputsGrant::new("oxide", 1_000)),
    }
}

/// One Oxide root the fixture created.
struct RootFixture {
    path: PathBuf,
    nonce: String,
    marker_digest: String,
    inode: u64,
    device: u64,
    preview: String,
}

impl Fixture {
    /// One registry intent, exactly as Oxide's journal entry is spelled.
    fn intent_value(&self, root: &RootFixture, caller_id: &str, phase: &str) -> serde_json::Value {
        let quarantine = root
            .path
            .parent()
            .expect("the root has a parent")
            .join(format!(".prune-{caller_id}"));
        json!({
            "version": OXIDE_INTENT_V1,
            "id": caller_id,
            "identity": {
                "checkout_id": CHECKOUT_ID,
                "checkout": self.checkout,
                "receipt": self.account.home.join("receipt.json"),
                "member": "target",
                "root": root.path,
                "root_kind": "external",
                "volume_uuid": FIXTURE_UUID,
                "nonce": root.nonce,
                "generation": 1,
                "dev": root.device,
                "ino": root.inode,
                "marker_digest": root.marker_digest,
            },
            "phase": phase,
            "quarantine": quarantine,
            "preview_sha256": root.preview,
            "created_ms": 1_000,
            "updated_ms": 2_000,
        })
    }
}

/// The request every positive fixture call makes.
fn request() -> DelegatedRequest {
    DelegatedRequest {
        grant_generation: 1,
    }
}

fn run(fixture: &Fixture, intent: &LiveIntent) -> super::Outcome {
    run_with_facts(fixture, &fixture.volume.facts(), intent)
}

fn run_with_facts(fixture: &Fixture, facts: &FakeVolume, intent: &LiveIntent) -> super::Outcome {
    run_request(fixture, facts, intent, &request())
}

fn run_request(
    fixture: &Fixture,
    facts: &FakeVolume,
    intent: &LiveIntent,
    request: &DelegatedRequest,
) -> super::Outcome {
    let observer = fixture.volume.observer();
    let mounts = fixture.mounts_of();
    execute(&AdapterCall {
        state_paths: &fixture.volume.state,
        dirs: &platform(),
        facts,
        mounts: &mounts,
        observer: &observer,
        account: &fixture.account,
        intent,
        request,
        bounds: AdapterBounds::design(),
    })
}

/// One adapter call with a short lock wait, so a held lock answers inside the
/// test instead of waiting out the design's five seconds.
fn run_short_lock(fixture: &Fixture, intent: &LiveIntent) -> super::Outcome {
    let facts = fixture.volume.facts();
    let observer = fixture.volume.observer();
    let mounts = fixture.mounts_of();
    let request = request();
    execute(&AdapterCall {
        state_paths: &fixture.volume.state,
        dirs: &platform(),
        facts: &facts,
        mounts: &mounts,
        observer: &observer,
        account: &fixture.account,
        intent,
        request: &request,
        bounds: AdapterBounds {
            lock_wait: Duration::from_millis(400),
            ..AdapterBounds::design()
        },
    })
}

/// Publish one `barge.intent/2` record for the covered root, as the adapter's
/// own writer would have, at `phase`, with `caller_id`.
#[allow(clippy::too_many_arguments)]
fn publish_record(
    fixture: &Fixture,
    root: &RootFixture,
    intent_id: &str,
    caller_id: &str,
    phase: IntentPhase,
) -> IntentRecord {
    let request_digest = request()
        .digest(&fixture.live_intent(root, caller_id))
        .expect("the fixture request digests");
    let now = state::now_ms();
    let intent = IntentRecord {
        schema: INTENT_SCHEMA.to_owned(),
        intent_id: intent_id.to_owned(),
        volume_uuid: fixture.volume.uuid.clone(),
        mount_hint: fixture.volume.root.clone(),
        root_dir: String::new(),
        folder: "oxide-outputs".to_owned(),
        unit: IntentUnit {
            parent: String::new(),
            name: root
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("the root has a name")
                .to_owned(),
            kind: UnitKind::Directory,
            device: root.device,
            inode: root.inode,
            marker: Some(UnitMarker::new(
                MarkerFamily::OxideOutputs,
                root.marker_digest.clone(),
            )),
            root_id: None,
            capture_name: format!("{}{intent_id}", state::QUARANTINE_PREFIX),
            grant: UnitGrant::delegation(MarkerFamily::OxideOutputs),
        },
        plan_digest: request_digest.clone(),
        phase: IntentPhase::Intent,
        generation: 1,
        recorded_ms: now,
        phase_ms: now,
        captures: Vec::new(),
        foreign_captures: Vec::new(),
        identity: None,
        absence: None,
        caller_intent_id: Some(caller_id.to_owned()),
        caller_request_digest: Some(request_digest),
    };
    let mut locks = LockSet::new(fixture.volume.state.clone());
    let _volume = locks
        .lock_volume(&fixture.volume.uuid)
        .expect("the volume lock");
    let _root = locks
        .lock_unit(&crate::state::unit_lock_key(
            &fixture.volume.uuid,
            "",
            &intent.unit.name,
        ))
        .expect("the unit lock");
    let write = locks.begin_intent_write().expect("the intent step");
    let mut published = intent.clone();
    state::publish_intent(&write, &fixture.volume.state, &published).expect("the intent publishes");
    if phase != IntentPhase::Intent {
        published = state::advance_intent(&write, &fixture.volume.state, &published, phase)
            .expect("the phase advances");
    }
    published
}

/// Read back one record by id.
fn record_of(fixture: &Fixture, intent_id: &str) -> IntentRecord {
    state::read_intent(&fixture.volume.state, intent_id).expect("the record reads back")
}

// ---------------------------------------------------------------------------
// The covered class: the positive path.
// ---------------------------------------------------------------------------

#[test]
fn a_covered_root_is_deleted_and_the_report_binds_the_barge_intent() {
    let fixture = Fixture::new("adapter-positive");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    let intent = fixture.live_intent(&root, CALLER_ID);

    let outcome = run(&fixture, &intent);

    assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
    let intent_id = outcome
        .intent_id
        .as_deref()
        .expect("the binding is reported");
    assert_eq!(outcome.caller_intent_id, CALLER_ID);
    // The root is gone, its quarantine never outlives the settled intent.
    assert!(!root.path.exists(), "the root is deleted");
    let record = record_of(&fixture, intent_id);
    assert_eq!(record.phase, IntentPhase::Deleted);
    assert_eq!(record.caller_intent_id.as_deref(), Some(CALLER_ID));
    assert_eq!(
        record.caller_request_digest.as_deref(),
        Some(request().digest(&intent).expect("the digest").as_str())
    );
    assert_eq!(
        record.unit.marker.as_ref().map(|marker| marker.family),
        Some(MarkerFamily::OxideOutputs)
    );
    assert_eq!(record.unit.inode, root.inode);
    // The quarantine leaf the record names is `.barge-prune-<barge intent id>`,
    // and settling removed it.
    assert!(!fixture
        .outputs
        .join(record.unit.capture_name.clone())
        .exists());
}

#[test]
fn a_second_call_after_a_settled_deletion_reports_the_same_operation() {
    let fixture = Fixture::new("adapter-idempotent-terminal");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    let intent = fixture.live_intent(&root, CALLER_ID);
    let first = run(&fixture, &intent);
    assert_eq!(first.kind, OutcomeKind::Deleted);

    let second = run(&fixture, &intent);
    assert_eq!(second.kind, OutcomeKind::Deleted, "{second:?}");
    assert_eq!(second.intent_id, first.intent_id, "the same journal record");
    // No second unit was started: exactly one record names this caller id.
    let intents = state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
        .expect("the journal reads");
    assert_eq!(intents.records.len(), 1);
}

// ---------------------------------------------------------------------------
// The stored authority: declaration, grant, generation.
// ---------------------------------------------------------------------------

#[test]
fn an_undeclared_withdrawn_mismatched_or_stale_grant_is_refused_before_any_effect() {
    // Undeclared: the area record is removed entirely.
    let undeclared = Fixture::new("adapter-undeclared");
    std::fs::remove_file(
        crate::area::declaration_path(&undeclared.volume.state, AREA_NAMESPACE, AREA_NAME)
            .expect("the declaration path"),
    )
    .expect("the declaration is removed");
    let root = undeclared.root("root-one");
    undeclared.covered(&root, CALLER_ID);
    let outcome = run(&undeclared, &undeclared.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists(), "nothing was deleted");

    // Withdrawn: a re-declaration that omits the grant and bumps generation.
    let withdrawn = Fixture::new("adapter-withdrawn");
    let root = withdrawn.root("root-one");
    withdrawn.covered(&root, CALLER_ID);
    withdrawn.withdraw_grant();
    let outcome = run(&withdrawn, &withdrawn.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // A stored grant whose family field is not the covered one is a record
    // this build refuses to read as authority at all: unknown, which keeps
    // everything, never a guessed-at match.
    let mismatched = Fixture::new("adapter-grant-family");
    let root = mismatched.root("root-one");
    mismatched.covered(&root, CALLER_ID);
    let path = crate::area::declaration_path(&mismatched.volume.state, AREA_NAMESPACE, AREA_NAME)
        .expect("the declaration path");
    let stored = std::fs::read_to_string(&path).expect("the declaration reads");
    let corrupted = stored.replace(
        &format!("\"family\": \"{}\"", OutputsGrant::FAMILY),
        "\"family\": \"oxide_scratch\"",
    );
    std::fs::write(&path, corrupted).expect("the declaration is corrupted");
    let outcome = run(&mismatched, &mismatched.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(root.path.exists());

    // Stale: the caller recorded a generation newer than the stored record's.
    let stale = Fixture::new("adapter-stale-generation");
    let root = stale.root("root-one");
    stale.covered(&root, CALLER_ID);
    let intent = stale.live_intent(&root, CALLER_ID);
    let observer = stale.volume.observer();
    let mounts = stale.mounts_of();
    let stale_request = DelegatedRequest {
        grant_generation: 9,
    };
    let outcome = execute(&AdapterCall {
        state_paths: &stale.volume.state,
        dirs: &platform(),
        facts: &stale.volume.facts(),
        mounts: &mounts,
        observer: &observer,
        account: &stale.account,
        intent: &intent,
        request: &stale_request,
        bounds: AdapterBounds::design(),
    });
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(outcome
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("generation")));
    assert!(root.path.exists());
}

// ---------------------------------------------------------------------------
// The covered identity: marker, nonce, digest, owner.
// ---------------------------------------------------------------------------

#[test]
fn a_changed_marker_nonce_digest_or_owner_is_refused() {
    // Marker bytes changed after the claim: the digest no longer matches.
    let changed = Fixture::new("adapter-marker-changed");
    let root = changed.root("root-one");
    changed.covered(&root, CALLER_ID);
    std::fs::write(
        root.path.join(crate::reference::OXIDE_MARKER_FILE),
        format!(
            r#"{{"version":"{OXIDE_MARKER_V2}","checkout":"{}","checkout_id":"{CHECKOUT_ID}","label":"dev","recipe":"target-debug-v1","kind":"external","volume_uuid":"{FIXTURE_UUID}","nonce":"{NONCE}","generation":2,"created_ms":1000}}"#,
            changed.checkout.display(),
        ),
    )
    .expect("the marker is rewritten");
    let outcome = run(&changed, &changed.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists(), "the root with other bytes is kept");

    // Wrong nonce: the identity binds one nonce, the marker carries another.
    let nonce = Fixture::new("adapter-wrong-nonce");
    let root = nonce.root("root-one");
    nonce.covered(&root, CALLER_ID);
    let mut intent = nonce.live_intent(&root, CALLER_ID);
    intent.identity.nonce = Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned());
    let outcome = run(&nonce, &intent);
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // Wrong owner: the marker names another volume than the intent's.
    let owner = Fixture::new("adapter-wrong-owner");
    let root = owner.root("root-one");
    owner.covered(&root, CALLER_ID);
    let mut intent = owner.live_intent(&root, CALLER_ID);
    intent.identity.volume_uuid = Some(crate::testkit::OTHER_UUID.to_owned());
    let outcome = run(&owner, &intent);
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // A `/1` marker is not the covered class, whatever its bytes bind.
    let legacy_marker = Fixture::new("adapter-marker-v1");
    let root = legacy_marker.root("root-one");
    legacy_marker.covered(&root, CALLER_ID);
    std::fs::write(
        root.path.join(crate::reference::OXIDE_MARKER_FILE),
        r#"{"version":"oxide.outputs.root/1","checkout":"/c","checkout_id":"c","label":"l","recipe":"r","kind":"external","generation":1,"created_ms":1}"#,
    )
    .expect("the legacy marker is written");
    let outcome = run(&legacy_marker, &legacy_marker.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());
}

// ---------------------------------------------------------------------------
// The live intent: the stored registry is the permission.
// ---------------------------------------------------------------------------

#[test]
fn a_selected_mismatched_settled_or_absent_live_intent_is_refused() {
    // A root the registry still selects: the intent's identity names it, but
    // the caller's claim says `intent` while the journal shows a settled
    // phase.
    let settled = Fixture::new("adapter-settled-intent");
    let root = settled.root("root-one");
    settled.write_control_state(Some("oxide.outputs/3"), 4);
    settled.write_registry(
        &root,
        vec![settled.intent_value(&root, CALLER_ID, "deleted")],
    );
    let outcome = run(&settled, &settled.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // Identity mismatch: the stored intent names another inode.
    let mismatched = Fixture::new("adapter-intent-mismatch");
    let root = mismatched.root("root-one");
    mismatched.covered(&root, CALLER_ID);
    let mut intent = mismatched.live_intent(&root, CALLER_ID);
    intent.identity.ino = root.inode + 1;
    let outcome = run(&mismatched, &intent);
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // Preview mismatch: the stored intent authorised another preview.
    let preview = Fixture::new("adapter-preview-mismatch");
    let root = preview.root("root-one");
    preview.covered(&root, CALLER_ID);
    let mut intent = preview.live_intent(&root, CALLER_ID);
    intent.preview_sha256 = "cd".repeat(32);
    let outcome = run(&preview, &intent);
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");

    // The id is absent from a completely read registry: no live intent.
    let absent = Fixture::new("adapter-intent-absent");
    let root = absent.root("root-one");
    absent.write_control_state(Some("oxide.outputs/3"), 4);
    absent.write_registry(
        &root,
        vec![absent.intent_value(&root, OTHER_CALLER_ID, "intent")],
    );
    let outcome = run(&absent, &absent.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());
}

// ---------------------------------------------------------------------------
// Unknown authority: never a refusal that reads as absence.
// ---------------------------------------------------------------------------

#[test]
fn unreadable_or_unknown_authority_is_unknown_and_keeps_everything() {
    // The registry is there but is not the document Oxide writes.
    let unreadable = Fixture::new("adapter-registry-unreadable");
    let root = unreadable.root("root-one");
    unreadable.write_control_state(Some("oxide.outputs/3"), 4);
    let outputs = unreadable.control.join(crate::reference::OXIDE_OUTPUTS_DIR);
    std::fs::create_dir_all(&outputs).expect("Oxide's outputs folder is created");
    std::fs::write(
        outputs.join(crate::reference::OXIDE_REGISTRY_FILE),
        b"{ not json",
    )
    .expect("the garbage registry is written");
    let outcome = run(&unreadable, &unreadable.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(root.path.exists());

    // The registry reports itself incomplete.
    let incomplete = Fixture::new("adapter-registry-incomplete");
    let root = incomplete.root("root-one");
    incomplete.write_control_state(Some("oxide.outputs/3"), 4);
    let outputs = incomplete.control.join(crate::reference::OXIDE_OUTPUTS_DIR);
    std::fs::create_dir_all(&outputs).expect("Oxide's outputs folder is created");
    let document = json!({
        "version": OXIDE_REGISTRY_V2,
        "schema": 2,
        "revision": 1,
        "complete": false,
        "incomplete": {"reason": "mid-write", "since_ms": 1, "transaction": "t"},
        "created_by": "oxide",
        "rows": [],
        "intents": [incomplete.intent_value(&root, CALLER_ID, "intent")],
    });
    std::fs::write(
        outputs.join(crate::reference::OXIDE_REGISTRY_FILE),
        serde_json::to_vec_pretty(&document).expect("the registry encodes"),
    )
    .expect("the registry is written");
    let outcome = run(&incomplete, &incomplete.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(root.path.exists());

    // The registry is absent while a live intent is claimed: unknown, never
    // positive absence.
    let absent_registry = Fixture::new("adapter-registry-absent");
    let root = absent_registry.root("root-one");
    absent_registry.write_control_state(Some("oxide.outputs/3"), 4);
    let outcome = run(
        &absent_registry,
        &absent_registry.live_intent(&root, CALLER_ID),
    );
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(root.path.exists());

    // No control directory at all: the caller's claim cannot be contradicted,
    // so unknown.
    let no_control = Fixture::new("adapter-no-control");
    let root = no_control.root("root-one");
    let outcome = run(&no_control, &no_control.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(root.path.exists());
}

// ---------------------------------------------------------------------------
// The fence: legacy is refused, unknown is unknown.
// ---------------------------------------------------------------------------

#[test]
fn the_legacy_fence_is_not_covered_and_an_unknown_fence_is_unknown() {
    // The published whole-target `/2` fence: legacy, refused.
    let legacy = Fixture::new("adapter-legacy-fence");
    let root = legacy.root("root-one");
    legacy.write_control_state(Some("oxide.outputs/2"), 3);
    legacy.write_registry(&root, vec![legacy.intent_value(&root, CALLER_ID, "intent")]);
    let outcome = run(&legacy, &legacy.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // No fence at all: the pool never opted in.
    let fenceless = Fixture::new("adapter-no-fence");
    let root = fenceless.root("root-one");
    fenceless.write_control_state(None, 2);
    fenceless.write_registry(
        &root,
        vec![fenceless.intent_value(&root, CALLER_ID, "intent")],
    );
    let outcome = run(&fenceless, &fenceless.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // An unreadable control state: unknown, never not-covered.
    let unreadable = Fixture::new("adapter-fence-unreadable");
    let root = unreadable.root("root-one");
    unreadable.write_registry(
        &root,
        vec![unreadable.intent_value(&root, CALLER_ID, "intent")],
    );
    std::fs::create_dir_all(&unreadable.control).expect("the control directory is created");
    std::fs::write(unreadable.control.join("state.json"), b"{ truncated").expect("written");
    let outcome = run(&unreadable, &unreadable.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(root.path.exists());
}

// ---------------------------------------------------------------------------
// The single-deleter rule: no generic path touches this family.
// ---------------------------------------------------------------------------

#[test]
fn a_generic_host_release_never_deletes_or_settles_an_oxide_outputs_root() {
    let fixture = Fixture::new("adapter-generic-refusal");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    // Publish a pending intent exactly as the adapter would have.
    let record = publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Intent);

    // A direct library call is refused before any effect.
    let unit = DeletionUnit {
        volume_uuid: fixture.volume.uuid.clone(),
        root_dir: String::new(),
        folder: "oxide-outputs".to_owned(),
        parent: String::new(),
        name: record.unit.name.clone(),
        root_id: None,
        kind: UnitKind::Directory,
        device: crate::os::DeviceId::new(root.device),
        inode: root.inode,
        marker: Some(UnitMarker::new(
            MarkerFamily::OxideOutputs,
            root.marker_digest.clone(),
        )),
        grant: UnitGrant::delegation(MarkerFamily::OxideOutputs),
        plan_digest: "0".repeat(64),
    };
    let declaration = Fixture::granted_declaration();
    let error = crate::delete::delete_unit_on_volume(
        &fixture.volume.state,
        &platform(),
        &fixture.volume.facts(),
        &fixture.volume.mounts(),
        &fixture.volume.observer(),
        &declaration,
        &unit,
        crate::delete::DeleteBounds::default(),
    )
    .expect_err("a generic delete of the family is refused");
    assert_eq!(error.kind(), crate::error::ErrorKind::RefusedBoundary);
    assert!(root.path.exists(), "the generic path deleted nothing");

    // A generic replay settles nothing and rolls nothing back.
    let report = crate::delete::replay_pending(
        &fixture.volume.state,
        &platform(),
        &fixture.volume.facts(),
        &fixture.volume.observer(),
        crate::delete::DeleteBounds::default(),
        None,
    )
    .expect("the replay runs");
    assert_eq!(report.pending.len(), 1, "{report:?}");
    assert_eq!(report.pending[0].intent_id, record.intent_id);
    assert!(root.path.exists());
    assert_eq!(
        record_of(&fixture, &record.intent_id).phase,
        IntentPhase::Intent,
        "the generic replay changed no phase"
    );
}

// ---------------------------------------------------------------------------
// Custody: a live hold keeps the root.
// ---------------------------------------------------------------------------

#[test]
fn a_live_hold_keeps_the_covered_root() {
    let fixture = Fixture::new("adapter-live-hold");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    // A delegated unit's name is not a Barge root id, so its hold lives in the
    // unit-name hold directory, exactly where custody reads it. The hold is
    // private state: it must be owner-only, or custody answers unknown and
    // this test would cover the unknown branch instead of the held one.
    let hold_dir = fixture.volume.state.holds_dir().join("root-one");
    std::fs::create_dir_all(&hold_dir).expect("the hold directory is created");
    let hold_path = hold_dir.join("fixture-holder.json");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&hold_path)
            .expect("the hold is created");
        file.write_all(&crate::testkit::VolumeFixture::hold_record(
            "root-one",
            "fixture-holder",
            std::process::id(),
            7_000_000,
            crate::testkit::FIXTURE_BOOT,
        ))
        .expect("the hold is written");
    }
    let hold_mode = std::fs::metadata(&hold_path)
        .expect("the hold is statable")
        .permissions()
        .mode();
    assert_eq!(
        hold_mode & 0o077,
        0,
        "the fixture hold is not owner-only, so custody cannot decide it and this test covers the wrong branch"
    );
    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    // The refusal must be custody's live-hold fact, not an unrelated unknown:
    // a 0644 hold would answer unknown and must fail here.
    let reason = outcome.reason.clone().unwrap_or_default();
    assert!(
        reason.contains("custody is not proven") && reason.contains("is live"),
        "the refusal is not custody's live-hold fact: {reason}"
    );
    assert!(root.path.exists(), "a held root is kept");
    // No journal record: the refusal happened before any effect.
    let intents = state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
        .expect("the journal reads");
    assert!(
        intents
            .records
            .iter()
            .all(|record| record.caller_intent_id.is_none()),
        "no record names this caller id"
    );
}

// ---------------------------------------------------------------------------
// Volume facts: absent volume and changed device.
// ---------------------------------------------------------------------------

#[test]
fn an_absent_volume_refuses_and_a_remounted_device_still_binds() {
    // Unavailable volume: nothing is deleted, and the caller keeps pending.
    let absent = Fixture::new("adapter-volume-absent");
    let root = absent.root("root-one");
    absent.covered(&root, CALLER_ID);
    let facts = absent.volume.facts();
    facts.present.set(false);
    let outcome = run_with_facts(&absent, &facts, &absent.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());

    // A remount that changed the device number: the joint protocol's remount
    // row binds UUID + inode + nonce + marker bytes and treats the device as
    // the live observation. Both the caller's claim and Oxide's stored intent
    // carry the plan-time device, which no longer matches the live volume's;
    // the deletion proceeds on the verified live volume regardless.
    let remounted = Fixture::new("adapter-remount");
    let root = remounted.root("root-one");
    remounted.write_control_state(Some("oxide.outputs/3"), 4);
    let mut stored_intent = remounted.intent_value(&root, CALLER_ID, "intent");
    stored_intent["identity"]["dev"] = serde_json::json!(root.device + 5_000);
    remounted.write_registry(&root, vec![stored_intent]);
    let mut intent = remounted.live_intent(&root, CALLER_ID);
    intent.identity.dev = root.device + 5_000;
    let outcome = run(&remounted, &intent);
    assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
    assert!(!root.path.exists(), "the remounted root is deleted");
    let intents = state::read_intents(&remounted.volume.state, crate::contract::INTENT_ROWS)
        .expect("the journal reads");
    let record = &intents.records[0];
    // The journal records the device the live volume reported, not the
    // claim's stale one.
    assert_ne!(record.unit.device, intent.identity.dev);
}

// ---------------------------------------------------------------------------
// A truncated journal is unknown, never fresh and never no-effect.
// ---------------------------------------------------------------------------

#[test]
fn a_truncated_journal_is_unknown_and_never_known_no_effect() {
    let fixture = Fixture::new("adapter-truncated-journal");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    // Overfill the journal past its read cap with valid records, so the
    // adapter's scan cannot claim to have seen the whole world.
    std::fs::create_dir_all(fixture.volume.state.intents_dir())
        .expect("the journal directory is created");
    let uuid = fixture.volume.uuid.clone();
    for index in 0..=crate::contract::INTENT_ROWS {
        let intent_id = format!("{index:032x}");
        let record = IntentRecord {
            schema: INTENT_SCHEMA.to_owned(),
            intent_id: intent_id.clone(),
            volume_uuid: uuid.clone(),
            mount_hint: fixture.volume.root.clone(),
            root_dir: String::new(),
            folder: "oxide-outputs".to_owned(),
            unit: IntentUnit {
                parent: String::new(),
                name: format!("t{index:032x}"),
                kind: UnitKind::Directory,
                device: 0,
                inode: 1,
                marker: Some(UnitMarker::new(MarkerFamily::Barge, "a".repeat(64))),
                root_id: None,
                capture_name: format!("{}{intent_id}", state::QUARANTINE_PREFIX),
                grant: UnitGrant::native_root(format!("t{index:032x}")),
            },
            plan_digest: "b".repeat(64),
            phase: IntentPhase::Deleted,
            generation: 1,
            recorded_ms: 1,
            phase_ms: 1,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
            identity: None,
            absence: None,
            caller_intent_id: None,
            caller_request_digest: None,
        };
        let bytes = serde_json::to_vec_pretty(&record).expect("the record encodes");
        let path = state::intent_path(&fixture.volume.state, &intent_id).expect("the intent path");
        std::fs::write(path, bytes).expect("the record is written");
    }

    // The scan answers unknown and keeps everything: no second unit, no
    // no-effect fact, no deletion.
    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(outcome
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("records")));
    assert!(root.path.exists());
}

// ---------------------------------------------------------------------------
// The same-call rule: Locked is a fact only with nothing written.
// ---------------------------------------------------------------------------

#[test]
fn a_pre_intent_lock_refusal_is_known_no_effect_and_a_pending_intent_under_lock_is_pending() {
    let fixture = Fixture::new("adapter-locked-first-call");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    let intent = fixture.live_intent(&root, CALLER_ID);

    // Another holder keeps the volume's exclusive lock.
    let holder_state = fixture.volume.state.clone();
    let holder = std::thread::spawn(move || {
        let mut locks =
            LockSet::with_deadline(holder_state, Deadline::after(Duration::from_secs(60)));
        let lock = locks
            .lock_volume(&VolumeUuid::parse(FIXTURE_UUID).expect("the fixture UUID"))
            .expect("the holder lock");
        std::thread::sleep(Duration::from_secs(2));
        drop(lock);
    });
    std::thread::sleep(Duration::from_millis(300));

    // First call under the same id: this call wrote nothing, so its own
    // `Locked` return is the known no-effect fact.
    let outcome = run_short_lock(&fixture, &intent);
    assert_eq!(
        outcome.kind,
        OutcomeKind::KnownNoEffectLocked,
        "{outcome:?}"
    );
    assert!(root.path.exists());
    holder.join().expect("the holder finishes");

    // Now a pending intent exists for this caller id; a locked call is a
    // resumed one, and it stays pending.
    publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Intent);
    let holder_state = fixture.volume.state.clone();
    let holder = std::thread::spawn(move || {
        let mut locks =
            LockSet::with_deadline(holder_state, Deadline::after(Duration::from_secs(60)));
        let lock = locks
            .lock_volume(&VolumeUuid::parse(FIXTURE_UUID).expect("the fixture UUID"))
            .expect("the holder lock");
        std::thread::sleep(Duration::from_secs(2));
        drop(lock);
    });
    std::thread::sleep(Duration::from_millis(300));
    let outcome = run_short_lock(&fixture, &intent);
    assert_eq!(outcome.kind, OutcomeKind::Pending, "{outcome:?}");
    assert_eq!(outcome.intent_id.as_deref(), Some(INTENT_ID));
    holder.join().expect("the holder finishes");
}

// ---------------------------------------------------------------------------
// Recovery: pre-rename rollback, post-rename replay, partial, terminal.
// ---------------------------------------------------------------------------

#[test]
fn a_pre_rename_pending_intent_rolls_back_and_the_root_stays() {
    let fixture = Fixture::new("adapter-rollback");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Intent);

    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::RolledBack, "{outcome:?}");
    assert!(root.path.exists(), "the root is exactly where it was");
    // The record is removed, so a later lookup is positively absent.
    assert!(state::read_intent(&fixture.volume.state, INTENT_ID).is_err());
    assert_eq!(
        lookup(
            &fixture.volume.state,
            &platform(),
            &fixture.volume.facts(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &fixture.live_intent(&root, CALLER_ID),
            &request(),
            Some(INTENT_ID),
        ),
        BargeLookup::PositivelyAbsent
    );
}

#[test]
fn a_post_rename_pending_intent_resumes_to_completion() {
    let fixture = Fixture::new("adapter-resume-renamed");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    let record = publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Renamed);
    // The capture happened: the root sits under its quarantine name.
    std::fs::rename(
        &root.path,
        fixture.outputs.join(record.unit.capture_name.clone()),
    )
    .expect("the capture rename");

    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
    assert!(!fixture
        .outputs
        .join(record.unit.capture_name.clone())
        .exists());
    assert_eq!(record_of(&fixture, INTENT_ID).phase, IntentPhase::Deleted);
}

#[test]
fn a_partial_quarantine_stays_pending_and_names_what_stopped_it() {
    let fixture = Fixture::new("adapter-partial");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    let record = publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Renamed);
    // The captured leaf holds a symlink, which the walk never follows and
    // never deletes: the deletion stops and the intent stays pending.
    let quarantine = fixture.outputs.join(record.unit.capture_name.clone());
    std::fs::rename(&root.path, &quarantine).expect("the capture rename");
    std::os::unix::fs::symlink("/elsewhere", quarantine.join("entry")).expect("the symlink");

    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Pending, "{outcome:?}");
    assert!(quarantine.exists(), "the quarantine leaf is kept");
    assert_eq!(
        record_of(&fixture, INTENT_ID).phase,
        IntentPhase::Renamed,
        "no phase advanced past the stop"
    );
}

#[test]
fn a_resumed_call_refuses_a_settled_or_withdrawn_caller_and_a_different_request() {
    let fixture = Fixture::new("adapter-resume-guards");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Intent);

    // The caller's journal settled while Barge's intent is pending: the resume
    // refuses and keeps the record.
    fixture.write_registry(
        &root,
        vec![fixture.intent_value(&root, CALLER_ID, "resolved")],
    );
    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Refused, "{outcome:?}");
    assert!(root.path.exists());
    assert_eq!(
        record_of(&fixture, INTENT_ID).phase,
        IntentPhase::Intent,
        "the refusal changed nothing"
    );

    // A different request under the same caller id: conflicting recovery stays unknown.
    fixture.covered(&root, CALLER_ID);
    let observer = fixture.volume.observer();
    let mounts = fixture.mounts_of();
    let later_intent = fixture.live_intent(&root, CALLER_ID);
    let other_request = DelegatedRequest {
        grant_generation: 7,
    };
    let outcome = execute(&AdapterCall {
        state_paths: &fixture.volume.state,
        dirs: &platform(),
        facts: &fixture.volume.facts(),
        mounts: &mounts,
        observer: &observer,
        account: &fixture.account,
        intent: &later_intent,
        request: &other_request,
        bounds: AdapterBounds::design(),
    });
    assert_eq!(outcome.kind, OutcomeKind::Unknown, "{outcome:?}");
    assert!(outcome
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("binding")));
}

// ---------------------------------------------------------------------------
// The bound lookup: the four states.
// ---------------------------------------------------------------------------

#[test]
fn the_lookup_answers_the_four_states() {
    let fixture = Fixture::new("adapter-lookup");
    let root = fixture.root("root-one");

    // Nothing recorded, no leaf: positively absent.
    fixture.covered(&root, CALLER_ID);
    assert_eq!(
        lookup(
            &fixture.volume.state,
            &platform(),
            &fixture.volume.facts(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &fixture.live_intent(&root, CALLER_ID),
            &request(),
            None,
        ),
        BargeLookup::PositivelyAbsent
    );

    // Pending record: found, with its quarantine leaf's name.
    publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Intent);
    match lookup(
        &fixture.volume.state,
        &platform(),
        &fixture.volume.facts(),
        &fixture.volume.mounts(),
        &fixture.volume.observer(),
        &fixture.live_intent(&root, CALLER_ID),
        &request(),
        Some(INTENT_ID),
    ) {
        BargeLookup::FoundPending {
            intent_id,
            phase,
            quarantine_name,
        } => {
            assert_eq!(intent_id, INTENT_ID);
            assert_eq!(phase, "intent");
            assert_eq!(
                quarantine_name,
                format!("{}{INTENT_ID}", state::QUARANTINE_PREFIX)
            );
        }
        answer => panic!("expected found-pending, got {answer:?}"),
    }

    // Terminal record: found terminal.
    {
        let mut locks = LockSet::new(fixture.volume.state.clone());
        let _volume = locks
            .lock_volume(&fixture.volume.uuid)
            .expect("the volume lock");
        let _unit = locks
            .lock_unit(&crate::state::unit_lock_key(
                &fixture.volume.uuid,
                "",
                "root-one",
            ))
            .expect("the unit lock");
        let write = locks.begin_intent_write().expect("the intent step");
        state::advance_intent(
            &write,
            &fixture.volume.state,
            &record_of(&fixture, INTENT_ID),
            IntentPhase::Deleted,
        )
        .expect("the settle advances");
    }
    assert_eq!(
        lookup(
            &fixture.volume.state,
            &platform(),
            &fixture.volume.facts(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &fixture.live_intent(&root, CALLER_ID),
            &request(),
            Some(INTENT_ID),
        ),
        BargeLookup::FoundTerminal {
            intent_id: INTENT_ID.to_owned(),
        }
    );

    // No record for this caller id, but an orphan quarantine leaf: unknown,
    // because a leaf cannot be attributed without the binding.
    let orphan = Fixture::new("adapter-lookup-orphan");
    let root = orphan.root("root-one");
    orphan.covered(&root, OTHER_CALLER_ID);
    std::fs::create_dir_all(fixture_outputs_leaf(&orphan, INTENT_ID))
        .expect("the orphan leaf is created");
    assert!(matches!(
        lookup(
            &orphan.volume.state,
            &platform(),
            &orphan.volume.facts(),
            &orphan.volume.mounts(),
            &orphan.volume.observer(),
            &orphan.live_intent(&root, CALLER_ID),
            &request(),
            None,
        ),
        BargeLookup::Unknown { .. }
    ));

    // With the binding of another leaf, the orphan is not this operation's,
    // so the absence of this operation's record and leaf is positive.
    assert_eq!(
        lookup(
            &orphan.volume.state,
            &platform(),
            &orphan.volume.facts(),
            &orphan.volume.mounts(),
            &orphan.volume.observer(),
            &orphan.live_intent(&root, CALLER_ID),
            &request(),
            Some("e".repeat(32).as_str()),
        ),
        BargeLookup::PositivelyAbsent
    );
}

fn fixture_outputs_leaf(fixture: &Fixture, intent_id: &str) -> PathBuf {
    fixture
        .outputs
        .join(format!("{}{intent_id}", state::QUARANTINE_PREFIX))
}

// ---------------------------------------------------------------------------
// The old families and records still behave.
// ---------------------------------------------------------------------------

#[test]
fn the_old_families_and_old_intent_records_still_behave() {
    // A Barge-owned root still deletes through the generic path.
    let fixture = Fixture::new("adapter-old-families");
    let declaration = fixture.volume.declaration("pool");
    area::declare(&fixture.volume.state, &declaration, 1_000).expect("the area is declared");
    let (root, _resolved) = fixture.volume.create_root(
        &fixture.volume.record,
        &declaration,
        area::RootKind::AreaRoot,
    );
    let unit = DeletionUnit::from_owned(&root, "", &declaration.folder, &"0".repeat(64));
    crate::delete::delete_unit_on_volume(
        &fixture.volume.state,
        &platform(),
        &fixture.volume.facts(),
        &fixture.volume.mounts(),
        &fixture.volume.observer(),
        &declaration,
        &unit,
        crate::delete::DeleteBounds::default(),
    )
    .expect("the generic delete still works");

    // A v1 intent record still reads back, with no caller correlation.
    let v1 = json!({
        "schema": "barge.intent/1",
        "intent_id": format!("{:032x}", 9),
        "root_id": format!("{:032x}", 10),
        "volume_uuid": FIXTURE_UUID,
        "mount_hint": fixture.volume.root,
        "root_dir": "",
        "folder": "pool",
        "root_name": format!("{:032x}", 10),
        "quarantine_name": format!("{}{:032x}", state::QUARANTINE_PREFIX, 9),
        "device": fixture.volume.device.get(),
        "inode": 4_242,
        "marker_digest": "a".repeat(64),
        "plan_digest": "b".repeat(64),
        "phase": "intent",
        "generation": 1,
        "recorded_ms": 1,
        "phase_ms": 1,
    });
    let path =
        state::intent_path(&fixture.volume.state, &format!("{:032x}", 9)).expect("the intent path");
    std::fs::create_dir_all(fixture.volume.state.intents_dir()).expect("the journal exists");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&v1).expect("the v1 encodes"),
    )
    .expect("the v1 record is written");
    let read = state::read_intent(&fixture.volume.state, &format!("{:032x}", 9))
        .expect("the v1 record reads");
    assert_eq!(read.caller_intent_id, None);
    assert_eq!(
        read.unit.marker.as_ref().map(|marker| marker.family),
        Some(MarkerFamily::Barge)
    );
}

// ---------------------------------------------------------------------------
// The request digest binds every field the caller bounds.
// ---------------------------------------------------------------------------

#[test]
fn the_request_digest_changes_with_every_bound_field() {
    let fixture = Fixture::new("adapter-digest");
    let root = fixture.root("root-one");
    let intent = fixture.live_intent(&root, CALLER_ID);
    let base = request().digest(&intent).expect("the digest");
    let mut other_generation = request();
    other_generation.grant_generation = 2;
    assert_ne!(base, other_generation.digest(&intent).expect("the digest"));
    let mut other_identity = intent.clone();
    other_identity.identity.ino += 1;
    assert_ne!(base, request().digest(&other_identity).expect("the digest"));
    let mut other_preview = intent.clone();
    other_preview.preview_sha256 = "cd".repeat(32);
    assert_ne!(base, request().digest(&other_preview).expect("the digest"));
    let other_id = fixture.live_intent(&root, OTHER_CALLER_ID);
    assert_ne!(base, request().digest(&other_id).expect("the digest"));
}

// ---------------------------------------------------------------------------
// The claim's shape: malformed claims refuse before any read.
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_claim_refuses_before_any_read() {
    let fixture = Fixture::new("adapter-claim-shape");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);

    // A settled caller phase is never this adapter's.
    let mut settled = fixture.live_intent(&root, CALLER_ID);
    settled.phase = "deleted".to_owned();
    assert_eq!(run(&fixture, &settled).kind, OutcomeKind::Refused);

    // An id that is not 32 lowercase hex is refused.
    let mut bad_id = fixture.live_intent(&root, CALLER_ID);
    bad_id.id = "NOT AN ID".to_owned();
    assert_eq!(run(&fixture, &bad_id).kind, OutcomeKind::Refused);

    // The covered identity binds a nonce; a claim without one refuses.
    let mut no_nonce = fixture.live_intent(&root, CALLER_ID);
    no_nonce.identity.nonce = None;
    assert_eq!(run(&fixture, &no_nonce).kind, OutcomeKind::Refused);

    // The covered identity binds the volume UUID.
    let mut no_volume = fixture.live_intent(&root, CALLER_ID);
    no_volume.identity.volume_uuid = None;
    assert_eq!(run(&fixture, &no_volume).kind, OutcomeKind::Refused);
}

// Real journal records exercise the public lookup and execute entries. Each
// variant changes one dimension; even a valid stored digest cannot mask a
// different stored unit. No failed binding may write or start a second intent.
#[test]
fn the_recovery_checks_pending_and_terminal_operation_bindings() {
    for phase in [IntentPhase::Intent, IntentPhase::Deleted] {
        let fixture = Fixture::new("adapter-binding");
        let root = fixture.root("root-one");
        fixture.covered(&root, CALLER_ID);
        let record = publish_record(&fixture, &root, INTENT_ID, CALLER_ID, phase);
        let path = state::intent_path(&fixture.volume.state, INTENT_ID).unwrap();
        let original = std::fs::read(&path).unwrap();
        let intent = fixture.live_intent(&root, CALLER_ID);
        for dimension in 0..22 {
            let mut expected = intent.clone();
            let mut request = request();
            let mut stored = record.clone();
            let mut id = Some(INTENT_ID);
            match dimension {
                0 => {
                    expected.identity.volume_uuid =
                        Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".to_owned())
                }
                1 => id = Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
                2 => expected.identity.root = root.path.with_file_name("another-root"),
                3 => expected.identity.ino += 1,
                4 => expected.identity.nonce = Some("f".repeat(32)),
                5 => expected.identity.marker_digest = "f".repeat(64),
                6 => expected.preview_sha256 = "f".repeat(64),
                7 => request.grant_generation += 1,
                8 => expected.identity.volume_uuid = None,
                9 => {
                    stored.volume_uuid =
                        VolumeUuid::parse("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE").unwrap()
                }
                10 => stored.unit.name = "another-root".to_owned(),
                11 => stored.unit.inode += 1,
                12 => {
                    stored.unit.marker =
                        Some(UnitMarker::new(MarkerFamily::OxideOutputs, "f".repeat(64)))
                }
                13 => stored.folder = "another-area".to_owned(),
                14 => stored.caller_request_digest = Some("f".repeat(64)),
                15 => stored.caller_request_digest = None,
                16 => stored.plan_digest = "f".repeat(64),
                17 => {
                    stored.unit.marker = Some(UnitMarker::new(
                        MarkerFamily::OxideScratch,
                        root.marker_digest.clone(),
                    ))
                }
                18 => expected.preview_sha256 = "malformed".to_owned(),
                19 => expected.id = OTHER_CALLER_ID.to_owned(),
                20 => expected.identity.nonce = Some("malformed".to_owned()),
                21 => stored.caller_intent_id = Some(OTHER_CALLER_ID.to_owned()),
                _ => unreachable!(),
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&stored).unwrap()).unwrap();
            let before = std::fs::read(&path).unwrap();
            let answer = lookup(
                &fixture.volume.state,
                &platform(),
                &fixture.volume.facts(),
                &fixture.volume.mounts(),
                &fixture.volume.observer(),
                &expected,
                &request,
                id,
            );
            assert!(
                matches!(answer, BargeLookup::Unknown { .. }),
                "phase {phase:?} dimension {dimension}: {answer:?}"
            );
            // execute has no expected Barge id argument; that dimension belongs
            // to lookup only. All other mismatches must fail here too.
            if !matches!(dimension, 1 | 19 | 21) {
                let outcome = run_request(&fixture, &fixture.volume.facts(), &expected, &request);
                assert!(
                    matches!(outcome.kind, OutcomeKind::Unknown | OutcomeKind::Refused),
                    "phase {phase:?} dimension {dimension}: {outcome:?}"
                );
            }
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(root.path.exists());
            assert_eq!(
                std::fs::read_dir(fixture.volume.state.intents_dir())
                    .unwrap()
                    .count(),
                1
            );
        }
        std::fs::write(&path, &original).unwrap();
        // Device is an observation, excluded from durable correlation.
        let mut remounted = intent.clone();
        remounted.identity.dev += 5000;
        let answer = lookup(
            &fixture.volume.state,
            &platform(),
            &fixture.volume.facts(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &remounted,
            &request(),
            Some(INTENT_ID),
        );
        assert!(matches!(
            answer,
            BargeLookup::FoundPending { .. } | BargeLookup::FoundTerminal { .. }
        ));
        if phase == IntentPhase::Deleted {
            assert_eq!(run(&fixture, &remounted).kind, OutcomeKind::Deleted);
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
}

#[test]
fn duplicate_or_truncated_binding_is_unknown_without_writes() {
    for duplicate in [true, false] {
        let fixture = Fixture::new("adapter-binding-incomplete");
        let root = fixture.root("root-one");
        fixture.covered(&root, CALLER_ID);
        publish_record(&fixture, &root, INTENT_ID, CALLER_ID, IntentPhase::Deleted);
        let other = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        if duplicate {
            publish_record(&fixture, &root, other, CALLER_ID, IntentPhase::Intent);
        } else {
            std::fs::write(
                state::intent_path(&fixture.volume.state, other).unwrap(),
                b"{",
            )
            .unwrap();
        }
        let intent = fixture.live_intent(&root, CALLER_ID);
        assert!(matches!(
            lookup(
                &fixture.volume.state,
                &platform(),
                &fixture.volume.facts(),
                &fixture.volume.mounts(),
                &fixture.volume.observer(),
                &intent,
                &request(),
                Some(INTENT_ID),
            ),
            BargeLookup::Unknown { .. }
        ));
        assert_eq!(run(&fixture, &intent).kind, OutcomeKind::Unknown);
        assert!(root.path.exists());
        assert_eq!(
            std::fs::read_dir(fixture.volume.state.intents_dir())
                .unwrap()
                .count(),
            2
        );
    }
}
