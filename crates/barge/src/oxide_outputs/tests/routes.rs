//! Schema-3 route bindings are permission only for the exact immutable request.
use super::*;

/// Upgrade only the synthetic registry; Barge never writes this representation.
fn schema3(fixture: &Fixture, route: serde_json::Value) {
    fixture.write_control_state(Some("oxide.outputs/4"), 4);
    let path = fixture
        .control
        .join(crate::reference::OXIDE_OUTPUTS_DIR)
        .join(crate::reference::OXIDE_REGISTRY_FILE);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["version"] = json!("oxide.outputs.registry/3");
    document["schema"] = json!(3);
    document["deletion_routes"] = route;
    std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
}

#[test]
fn schema3_barge_route_requires_exact_immutable_request_binding() {
    let fixture = Fixture::new("adapter-schema3-barge");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    let hash = "a".repeat(64);
    schema3(
        &fixture,
        json!([{"intent_id":CALLER_ID,"deleter":"barge","request_sha256":hash}]),
    );
    let intent = fixture.live_intent(&root, CALLER_ID);
    let mut request = request();
    for wrong in [None, Some("b".repeat(64))] {
        request.request_sha256 = wrong;
        assert_eq!(
            run_request(&fixture, &fixture.volume.facts(), &intent, &request).kind,
            OutcomeKind::Refused
        );
        assert!(root.path.exists());
        assert!(
            state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
                .unwrap()
                .records
                .is_empty()
        );
    }
    request.request_sha256 = Some(hash);
    assert_eq!(
        run_request(&fixture, &fixture.volume.facts(), &intent, &request).kind,
        OutcomeKind::Deleted
    );
    assert!(!root.path.exists());
    request.request_sha256 = Some("b".repeat(64));
    assert_eq!(
        run_request(&fixture, &fixture.volume.facts(), &intent, &request).kind,
        OutcomeKind::Refused
    );
}

#[test]
fn schema3_local_missing_and_invalid_routes_never_delete() {
    for (label, routes) in [
        ("local", json!([{"intent_id":CALLER_ID,"deleter":"local"}])),
        ("missing", json!([])),
        (
            "invalid",
            json!([{"intent_id":CALLER_ID,"deleter":"barge","request_sha256":"bad"}]),
        ),
    ] {
        let fixture = Fixture::new(label);
        let root = fixture.root("root-one");
        fixture.covered(&root, CALLER_ID);
        schema3(&fixture, routes);
        let intent = fixture.live_intent(&root, CALLER_ID);
        let result = run(&fixture, &intent);
        assert!(matches!(
            result.kind,
            OutcomeKind::Unknown | OutcomeKind::Refused
        ));
        assert!(root.path.exists());
        assert!(
            state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
                .unwrap()
                .records
                .is_empty()
        );
    }
}

/// A terminal Barge record is evidence only for the same stored Oxide operation.
#[test]
fn terminal_recovery_checks_the_stored_operation_and_tolerates_remount_devices() {
    for routed in [false, true] {
        let fixture = Fixture::new("terminal-stored-binding");
        let root = fixture.root("root-one");
        fixture.covered(&root, CALLER_ID);
        let mut request = request();
        if routed {
            request.request_sha256 = Some("a".repeat(64));
            schema3(
                &fixture,
                json!([{"intent_id":CALLER_ID,"deleter":"barge","request_sha256":request.request_sha256}]),
            );
        }
        let intent = fixture.live_intent(&root, CALLER_ID);
        let deleted = run_request(&fixture, &fixture.volume.facts(), &intent, &request);
        assert_eq!(deleted.kind, OutcomeKind::Deleted);
        let journal_path =
            state::intent_path(&fixture.volume.state, deleted.intent_id.as_deref().unwrap())
                .unwrap();
        let journal = std::fs::read(&journal_path).unwrap();
        let registry_path = fixture
            .control
            .join(crate::reference::OXIDE_OUTPUTS_DIR)
            .join(crate::reference::OXIDE_REGISTRY_FILE);
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        let mut variants = Vec::new();
        for phase in ["renamed", "deleting", "marker_last", "deleted", "resolved"] {
            let mut changed = original.clone();
            changed["intents"][0]["phase"] = json!(phase);
            variants.push(changed);
        }
        let mut preview = original.clone();
        preview["intents"][0]["preview_sha256"] = json!("b".repeat(64));
        variants.push(preview);
        for (field, value) in [
            ("checkout_id", json!("b".repeat(64))),
            (
                "checkout",
                json!(fixture.checkout.with_file_name("other-checkout")),
            ),
            (
                "receipt",
                json!(fixture.account.home.join("other-receipt.json")),
            ),
            ("member", json!("other-member")),
            ("root", json!(root.path.with_file_name("other-root"))),
            ("root_kind", json!("local")),
            ("volume_uuid", json!("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")),
            ("nonce", json!("b".repeat(32))),
            ("generation", json!(2)),
            ("ino", json!(root.inode + 1)),
            ("marker_digest", json!("b".repeat(64))),
        ] {
            let mut changed = original.clone();
            changed["intents"][0]["identity"][field] = value;
            variants.push(changed);
        }
        for changed in variants {
            let bytes = serde_json::to_vec(&changed).unwrap();
            std::fs::write(&registry_path, &bytes).unwrap();
            let result = run_request(&fixture, &fixture.volume.facts(), &intent, &request);
            assert_eq!(result.kind, OutcomeKind::Refused, "{changed}: {result:?}");
            assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
            assert_eq!(std::fs::read(&registry_path).unwrap(), bytes);
            assert!(!root.path.exists());
        }
        let mut observed = original;
        observed["intents"][0]["identity"]["dev"] = json!(intent.identity.dev + 1000);
        std::fs::write(&registry_path, serde_json::to_vec(&observed).unwrap()).unwrap();
        let mut remounted = intent;
        remounted.identity.dev += 5000;
        assert_eq!(
            run_request(&fixture, &fixture.volume.facts(), &remounted, &request).kind,
            OutcomeKind::Deleted
        );
        assert_eq!(std::fs::read(journal_path).unwrap(), journal);
        assert!(!root.path.exists());
    }
}

/// The transaction both controls record ready in the coordinated fixtures.
const TRANSACTION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Publish the coordinated `/4` image over the fixture's `/2` registry,
/// keeping its row and its live intent, and created by `transaction`.
fn coordinated_image(fixture: &Fixture, transaction: &str) {
    let path = fixture
        .control
        .join(crate::reference::OXIDE_OUTPUTS_DIR)
        .join(crate::reference::OXIDE_REGISTRY_FILE);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["version"] = json!(crate::reference::OXIDE_REGISTRY_V4);
    document["schema"] = json!(4);
    document["created_by"] = json!(transaction);
    document["settled"] = json!([]);
    std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
}

#[test]
fn the_coordinated_handoff_refuses_the_delegated_route() {
    // The actual emitted single-authority handoff: both controls `/5` ready
    // under the transaction, and the `/4` image that transaction created. The
    // `/4` registry carries no deletion route, and the predecessor join this
    // path implements is not that class's authority, so recognising the fence
    // refuses rather than deleting through the old route.
    let fixture = Fixture::new("adapter-coordinated-ready");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    fixture.write_pool_controls(
        crate::pool_custody::COORDINATED_OWNED_OUTPUTS_FENCE,
        json!({"ready": {"transaction": TRANSACTION, "ready_ms": 1}}),
    );
    coordinated_image(&fixture, TRANSACTION);
    let intent = fixture.live_intent(&root, CALLER_ID);
    let result = run(&fixture, &intent);
    assert_eq!(result.kind, OutcomeKind::Refused, "{result:?}");
    assert!(root.path.exists());
    assert!(
        state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
            .unwrap()
            .records
            .is_empty()
    );
}

#[test]
fn a_one_sided_coordinated_adoption_never_authorises_the_delegated_route() {
    // Mid-adoption on the controls: the internal control records `/5` ready
    // while the removable control still says `/4`, with the `/4` image the
    // record names. The pair establishes nothing, so the answer is unknown and
    // the root is kept.
    let fixture = Fixture::new("adapter-coordinated-one-sided-controls");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    fixture.write_control_state(
        Some(crate::pool_custody::COORDINATED_OWNED_OUTPUTS_FENCE),
        4,
    );
    {
        let path = fixture.control.join("state.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["outputs_init"] = json!({"ready": {"transaction": TRANSACTION, "ready_ms": 1}});
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
    }
    let removable = fixture
        .control
        .join(crate::pool_custody::REMOVABLE_POOL_DIR)
        .join(crate::inspect::OXIDE_CONTROL_DIR);
    std::fs::create_dir_all(&removable).expect("the removable control directory is created");
    std::fs::write(
        removable.join("state.json"),
        serde_json::to_vec(&json!({
            "version": 9,
            "next_id": 1,
            "next_use": 1,
            "tickets": [],
            "cleans": [],
            "records": [],
            "owned_outputs": crate::pool_custody::ROUTED_OWNED_OUTPUTS_FENCE,
            "outputs_init": {"ready": {"transaction": TRANSACTION, "ready_ms": 1}},
            "recorded_accounting": "oxide.accounting/1",
            "eviction_accounting": "oxide.eviction-accounting/1",
            "slot_ceiling": "oxide.slots/256",
            "holding": "oxide.holding/1",
        }))
        .unwrap(),
    )
    .expect("the removable control state is written");
    coordinated_image(&fixture, TRANSACTION);
    let intent = fixture.live_intent(&root, CALLER_ID);
    let result = run(&fixture, &intent);
    assert_eq!(result.kind, OutcomeKind::Unknown, "{result:?}");
    assert!(root.path.exists());

    // One-sided on the image instead: the published `/4` control pair beside
    // the `/4` image that only the coordinated handoff creates. No pool control
    // records `/5`, so the image is unknown and nothing is deleted.
    let fixture = Fixture::new("adapter-coordinated-image-only");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    fixture.write_control_state(Some(crate::pool_custody::ROUTED_OWNED_OUTPUTS_FENCE), 4);
    coordinated_image(&fixture, TRANSACTION);
    let intent = fixture.live_intent(&root, CALLER_ID);
    let result = run(&fixture, &intent);
    assert_eq!(result.kind, OutcomeKind::Unknown, "{result:?}");
    assert!(root.path.exists());
    assert!(
        state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
            .unwrap()
            .records
            .is_empty()
    );
}
