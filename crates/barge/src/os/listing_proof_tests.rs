//! The real proof-only endpoint refuses before any kernel process read when
//! its guarded fixture binding is absent, malformed or escaped.

// Assertion failures are test failures; shipped code keeps the crate-wide denies.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;

#[test]
fn listing_proof_guard_requires_the_owned_child_and_fixture_before_observation() {
    let names = [
        "OXIDE_TASK_SCRATCH_ROOT",
        "OXIDE_PROOF_EVIDENCE",
        "OXIDE_PROOF_DEADLINE_NS",
        "BARGE_LISTING_PROOF_ROOT",
        "BARGE_LISTING_PROOF_PID",
    ];
    let saved: Vec<_> = names
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect();
    let parent = PathBuf::from(std::env::var_os("TMPDIR").expect("fresh caller fixture"));
    let owned = parent.join("listing-guard");
    let scratch = owned.join("scratch");
    let evidence = owned.join("evidence");
    let fixture = scratch.join("fixture");
    for path in [&owned, &scratch, &evidence, &fixture] {
        std::fs::create_dir(path).expect("fresh synthetic directory");
    }
    for name in names {
        std::env::remove_var(name);
    }
    assert!(MacOs.proof_descriptor_listing(0, &fixture, 1, 1).is_err());
    std::env::set_var("OXIDE_TASK_SCRATCH_ROOT", &scratch);
    std::env::set_var("OXIDE_PROOF_EVIDENCE", &evidence);
    std::env::set_var("OXIDE_PROOF_DEADLINE_NS", "1");
    std::env::set_var("BARGE_LISTING_PROOF_ROOT", &fixture);
    std::env::set_var("BARGE_LISTING_PROOF_PID", "0");
    // A valid synthetic routing context reaches argument refusal, still before
    // kinfo_process. No successful kernel observation is activated here.
    assert!(listing_proof_context(0, &fixture).is_ok());
    assert_eq!(
        MacOs
            .proof_descriptor_listing(0, &fixture, 1, 1)
            .expect_err("zero PID refuses")
            .kind(),
        ErrorKind::Usage
    );
    assert!(listing_proof_context(1, &fixture).is_err());
    assert!(listing_proof_context(0, &scratch).is_err());
    std::env::set_var("OXIDE_PROOF_DEADLINE_NS", "0");
    assert!(listing_proof_context(0, &fixture).is_err());
    std::env::set_var("OXIDE_PROOF_DEADLINE_NS", "1");
    std::env::set_var("OXIDE_PROOF_EVIDENCE", &fixture);
    assert!(listing_proof_context(0, &fixture).is_err());
    for (name, value) in saved {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
    // Caller-owned evidence is retained. No fixture cleanup occurs here.
}
