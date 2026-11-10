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
