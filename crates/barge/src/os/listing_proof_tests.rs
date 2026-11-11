//! The real proof-only endpoint refuses before any kernel process read when
//! its guarded fixture binding is absent, malformed or escaped.

// Assertion failures are test failures; shipped code keeps the crate-wide denies.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;

#[test]
fn listing_proof_inventory_reports_the_actual_sorted_set_and_all_types() {
    let fds = [
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 6,
        },
        libc::proc_fdinfo {
            proc_fd: 1,
            proc_fdtype: 1,
        },
        libc::proc_fdinfo {
            proc_fd: 2,
            proc_fdtype: 1,
        },
        libc::proc_fdinfo {
            proc_fd: 8,
            proc_fdtype: u32::MAX,
        },
    ];
    let ListingProofInventory {
        numbers,
        type_counts: types,
    } = listing_proof_fd_inventory(&fds).expect("valid actual rows");
    assert_eq!(numbers, vec![0, 1, 2, 8]);
    assert_eq!(types, vec![(1, 2), (6, 1), (u32::MAX, 1)]);
    let ListingProofInventory {
        numbers,
        type_counts: types,
    } = listing_proof_fd_inventory(&[]).expect("empty sample");
    assert!(numbers.is_empty() && types.is_empty());
}

#[test]
fn listing_proof_inventory_refuses_malformed_sets_without_trimming() {
    for numbers in [vec![-1], vec![2, 2], vec![4, 3]] {
        let fds: Vec<_> = numbers
            .into_iter()
            .map(|proc_fd| libc::proc_fdinfo {
                proc_fd,
                proc_fdtype: 1,
            })
            .collect();
        assert_eq!(
            listing_proof_fd_inventory(&fds)
                .expect_err("malformed set")
                .kind(),
            ErrorKind::UnknownState
        );
    }
    let excessive = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 1
        };
        descriptor_listing::MAX_ROWS + 1
    ];
    assert_eq!(
        listing_proof_fd_inventory(&excessive)
            .expect_err("original row ceiling")
            .kind(),
        ErrorKind::ResourceBound
    );
}

#[test]
fn listing_proof_exact_inventory_fits_the_fixed_case_and_refuses_report_overflow() {
    let fds: Vec<_> = (0..5003)
        .map(|proc_fd| libc::proc_fdinfo {
            proc_fd,
            proc_fdtype: 1,
        })
        .collect();
    let ListingProofInventory {
        numbers,
        type_counts: types,
    } = listing_proof_fd_inventory(&fds).expect("fixed large case");
    let report = serde_json::json!({"fd_numbers":numbers, "fd_type_counts":types, "matching_fd_numbers":(3..5003).collect::<Vec<_>>()});
    listing_proof_report_bound(&report).expect("lossless fixed case under 64 KiB");
    assert!(serde_json::to_vec(&report).expect("report bytes").len() < 60_000);
    // The exact byte boundary includes the driver's newline. No report field
    // or fd is truncated to obtain an answer.
    listing_proof_report_bound(&serde_json::Value::String("x".repeat(65_533)))
        .expect("exact 64 KiB with quotes/newline");
    assert_eq!(
        listing_proof_report_bound(&serde_json::Value::String("x".repeat(65_534)))
            .expect_err("one byte over")
            .kind(),
        ErrorKind::ResourceBound
    );
    let sparse: Vec<_> = (0..8191)
        .map(|offset| libc::proc_fdinfo {
            proc_fd: i32::MAX - 8190 + offset,
            proc_fdtype: 1,
        })
        .collect();
    let ListingProofInventory {
        numbers,
        type_counts: types,
    } = listing_proof_fd_inventory(&sparse).expect("representable nonnegative rows");
    assert_eq!(
        listing_proof_report_bound(
            &serde_json::json!({"fd_numbers":numbers,"fd_type_counts":types})
        )
        .expect_err("unrepresentable within report ceiling")
        .kind(),
        ErrorKind::ResourceBound
    );
}

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
