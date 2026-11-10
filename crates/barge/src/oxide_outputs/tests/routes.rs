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
