//! Caller and kernel-adapter regressions. Raw answers enter KernelListing;
//! production allocation, validation, identity, accounting and settlement run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn self_identity() -> ProcessIdentity {
    let pid = std::process::id();
    ProcessIdentity {
        pid,
        effective_uid: Some(MacOs.effective_uid().expect("own effective uid")),
        start: MacOs
            .process_start(pid)
            .expect("own start lookup")
            .expect("this process exists"),
    }
}

fn request(root: &Path, rows: usize) -> DescriptorScanRequest<'_> {
    DescriptorScanRequest {
        root,
        root_device: DeviceId::new(1),
        root_inode: 1,
        max_processes: 8,
        max_descriptors: rows,
        timeout: Duration::from_secs(10),
        self_exclusion: None,
    }
}

fn scratch() -> PathBuf {
    PathBuf::from(std::env::var_os("TMPDIR").expect("caller-selected scratch"))
}

fn raw(rows: Vec<(i32, u32)>) -> (i32, i32, Vec<(i32, u32)>) {
    let bytes = rows.len() * std::mem::size_of::<libc::proc_fdinfo>();
    (
        i32::try_from(bytes).expect("small fixture byte count"),
        0,
        rows,
    )
}

fn rows(count: usize) -> Vec<(i32, u32)> {
    (0..count)
        .map(|fd| (i32::try_from(fd).expect("small fixture fd"), 0))
        .collect()
}

fn vnode(fd: i32) -> (i32, u32) {
    (
        fd,
        u32::try_from(libc::PROX_FDTYPE_VNODE).expect("vnode type"),
    )
}

fn closed() -> StatedVnodeRead {
    StatedVnodeRead::new(StatedCall::Closed, StatedCall::Closed)
}

#[test]
fn a_dead_to_closed_pair_cannot_reset_its_original_local_window() {
    let identity = self_identity();
    let mut budget =
        ScanBudget::new(Instant::now(), 10, Duration::from_secs(10)).expect("original scan budget");
    let mut faults = [StatedListing {
        pid: identity.pid,
        ..StatedListing::default()
    }];
    let answer = MacOs.settle_pair(
        identity,
        0,
        0,
        &mut budget,
        &[],
        &mut faults,
        SettleFrom::ClosedAfterDead,
        Some(Instant::now()),
    );
    assert!(matches!(answer, Observed::Unreadable(reason) if reason.contains("window")));
    assert_eq!(budget.remaining, 10);
    assert_eq!(faults[0].list_calls, 0);
}

#[test]
fn initial_listing_refuses_zero_and_exact_full_but_accepts_short() {
    let identity = self_identity();
    let root = scratch();
    for (allowance, expected_calls) in [(0, 0), (1, 1), (2, 1)] {
        let mut faults = [StatedListing {
            pid: identity.pid,
            raw_listings: vec![raw(rows(1))],
            ..StatedListing::default()
        }];
        let answer = MacOs
            .scan_open_descriptors_with(&request(&root, allowance), &[identity], &[], &mut faults)
            .expect("scan answer");
        assert_eq!(faults[0].list_calls, expected_calls, "{answer:?}");
        if allowance == 2 {
            assert!(
                matches!(
                    answer,
                    DescriptorScan::Complete {
                        processes: 1,
                        descriptors: 1,
                        ..
                    }
                ),
                "{answer:?}"
            );
        } else {
            assert!(
                matches!(
                    answer,
                    DescriptorScan::Bounded {
                        processes: 0,
                        descriptors: 0,
                        bound: "descriptor work",
                        ..
                    }
                ),
                "{answer:?}"
            );
        }
    }
}

#[test]
fn grown_listing_counts_final_rows_once_in_public_evidence() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![raw(rows(4096)), raw(rows(4097))],
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(&request(&root, 16_384), &[identity], &[], &mut faults)
        .expect("scan answer");
    assert!(
        matches!(
            answer,
            DescriptorScan::Complete {
                processes: 1,
                descriptors: 4097,
                ..
            }
        ),
        "discarded prefix is work, not observed evidence: {answer:?}"
    );
    assert_eq!(faults[0].list_calls, 2);
}

#[test]
fn caller_cannot_enlarge_aggregate_row_or_time_ceilings() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: [4096, 8192, 16_384, 32_768]
            .into_iter()
            .map(|count| raw(rows(count)))
            .collect(),
        ..StatedListing::default()
    }];
    let mut requested = request(&root, usize::MAX);
    requested.timeout = Duration::MAX;
    let answer = MacOs
        .scan_open_descriptors_with(&requested, &[identity], &[], &mut faults)
        .expect("scan answer");
    assert!(
        matches!(
            answer,
            DescriptorScan::Bounded {
                bound: "descriptor work",
                processes: 0,
                descriptors: 0,
                ..
            }
        ),
        "a large request cannot buy a fifth listing: {answer:?}"
    );
    assert_eq!(faults[0].list_calls, 4);
    assert_eq!(
        ScanBudget::new(Instant::now(), usize::MAX, Duration::MAX)
            .expect("clamped deadline is representable")
            .timeout,
        Duration::from_secs(10)
    );
}

#[test]
fn process_allowance_is_checked_before_another_same_uid_listing() {
    let identity = self_identity();
    let root = scratch();
    for allowance in [0, 1] {
        let mut faults = [StatedListing {
            pid: identity.pid,
            raw_listings: vec![raw(Vec::new()), raw(Vec::new())],
            ..StatedListing::default()
        }];
        let mut requested = request(&root, 8);
        requested.max_processes = allowance;
        let answer = MacOs
            .scan_open_descriptors_with(&requested, &[identity, identity], &[], &mut faults)
            .expect("scan answer");
        assert!(
            matches!(answer, DescriptorScan::Bounded {
            processes, descriptors: 0, bound: "processes", ..
        } if processes == allowance),
            "{answer:?}"
        );
        assert_eq!(faults[0].list_calls, allowance);
        assert_eq!(faults[0].raw_listings.len(), 2 - allowance);
    }
}

#[test]
fn eperm_retry_uses_rows_left_by_the_initial_process() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![raw(rows(1)), (0, libc::EPERM, Vec::new()), raw(rows(2))],
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(
            &request(&root, 3),
            &[identity, identity],
            &[Duration::ZERO],
            &mut faults,
        )
        .expect("scan answer");
    let DescriptorScan::Bounded {
        processes,
        descriptors,
        bound,
        retry,
    } = answer
    else {
        panic!("re-observation cannot reset the original allowance: {answer:?}");
    };
    assert_eq!((processes, descriptors, bound), (1, 1, "descriptor work"));
    assert_eq!(faults[0].list_calls, 3);
    assert_eq!(retry.retried_unreadable, 1);
    assert_eq!(retry.retried[0].errnos, vec![libc::EPERM]);
    assert_eq!(retry.retried[0].resolution, RetryResolution::Bounded);
}

#[test]
fn closed_relisting_spends_shared_work_without_inflating_observed_count() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![raw(vec![vnode(5), (7, 0)]), raw(vec![(7, 0)])],
        vnode_reads: vec![(0, closed())],
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(&request(&root, 4), &[identity], &[], &mut faults)
        .expect("scan answer");
    assert!(
        matches!(answer, DescriptorScan::Complete {
        processes: 1, descriptors: 2, ref open, ..
    } if open.is_empty()),
        "the complete relist omits the closed fd: {answer:?}"
    );
    assert_eq!(faults[0].list_calls, 2);
}

#[test]
fn closed_relisting_refuses_when_the_remaining_capacity_fills() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![raw(vec![vnode(5)]), raw(vec![(7, 0)])],
        vnode_reads: vec![(0, closed())],
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(&request(&root, 2), &[identity], &[], &mut faults)
        .expect("scan answer");
    assert!(
        matches!(
            answer,
            DescriptorScan::Bounded {
                processes: 1,
                descriptors: 1,
                bound: "descriptor work",
                ..
            }
        ),
        "a filled relist proves no omission: {answer:?}"
    );
    assert_eq!(faults[0].list_calls, 2);
}

#[test]
fn exhausted_closed_relisting_refuses_before_a_kernel_read() {
    let identity = self_identity();
    let mut budget = ScanBudget::new(Instant::now(), 0, Duration::from_secs(10)).expect("deadline");
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![raw(Vec::new())],
        vnode_reads: vec![(0, closed())],
        ..StatedListing::default()
    }];
    let answer = MacOs.observe_descriptor(identity, 5, 0, &mut budget, &[], &mut faults);
    assert!(
        matches!(answer, Observed::Bounded("descriptor work")),
        "{answer:?}"
    );
    assert_eq!(faults[0].list_calls, 0);
    assert_eq!(faults[0].raw_listings.len(), 1);
}

#[test]
fn dead_to_closed_relisting_keeps_the_original_row_allowance() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![raw(vec![vnode(5)]), raw(vec![(7, 0)])],
        vnode_reads: vec![
            (0, StatedVnodeRead::dead_once()),
            (
                0,
                StatedVnodeRead::once(StatedCall::Closed, StatedCall::Closed),
            ),
            (0, closed()),
        ],
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(
            &request(&root, 2),
            &[identity],
            &[Duration::ZERO],
            &mut faults,
        )
        .expect("scan answer");
    assert!(
        matches!(
            answer,
            DescriptorScan::Bounded {
                processes: 1,
                descriptors: 1,
                bound: "descriptor work",
                ..
            }
        ),
        "dead-handle settlement cannot replenish relist work: {answer:?}"
    );
    assert_eq!(faults[0].list_calls, 2);
    assert!(
        faults[0].identity_reads >= 3,
        "the dead and closed settlements ran"
    );
}

#[test]
fn eperm_replacement_identity_reaches_the_per_fd_dead_handle_checks() {
    let identity = self_identity();
    let replacement = identity.start.checked_add(1).expect("fixture start");
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        start: Some(replacement),
        raw_listings: vec![(0, libc::EPERM, Vec::new()), raw(vec![vnode(5)])],
        vnode_reads: vec![(0, StatedVnodeRead::dead())],
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(
            &request(&root, 4),
            &[identity],
            &[Duration::ZERO],
            &mut faults,
        )
        .expect("scan answer");
    let DescriptorScan::Complete {
        dead_handles,
        retry,
        ..
    } = answer
    else {
        panic!("the verified replacement must be used downstream: {answer:?}");
    };
    assert_eq!(dead_handles.handles, 1);
    assert_eq!(dead_handles.records[0].start, replacement);
    assert_eq!(retry.retried[0].identity_changes, 1);
    assert_eq!(faults[0].list_calls, 2);
}

#[test]
fn eperm_replacement_identity_reaches_the_platform_check() {
    let identity = self_identity();
    let replacement = identity.start.checked_add(1).expect("fixture start");
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        start: Some(replacement),
        raw_listings: vec![(0, libc::EPERM, Vec::new()), raw(vec![vnode(5)])],
        vnode_reads: vec![(0, StatedVnodeRead::opaque())],
        platform: Some(StatedPlatform::IdentityChanged {
            command: "fixture".to_owned(),
            start: identity.start,
        }),
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(
            &request(&root, 4),
            &[identity],
            &[Duration::ZERO],
            &mut faults,
        )
        .expect("scan answer");
    let DescriptorScan::Unreadable { reason, .. } = answer else {
        panic!("a changed platform identity still refuses: {answer:?}");
    };
    assert!(
        reason.contains(&format!("from {replacement} to {}", identity.start)),
        "{reason}"
    );
    assert_eq!(faults[0].list_calls, 2);
}

#[test]
fn malformed_initial_answer_is_not_erased_by_later_gone_settlement() {
    let identity = self_identity();
    let root = scratch();
    let mut faults = [StatedListing {
        pid: identity.pid,
        raw_listings: vec![(1, 0, Vec::new())],
        gone_after: Some(0),
        ..StatedListing::default()
    }];
    let answer = MacOs
        .scan_open_descriptors_with(&request(&root, 4), &[identity], &[], &mut faults)
        .expect("scan answer");
    assert!(
        matches!(answer, DescriptorScan::Unreadable { .. }),
        "{answer:?}"
    );
    assert_eq!(faults[0].list_calls, 1);
    assert_eq!(
        faults[0].identity_reads, 0,
        "unrelated failure has precedence"
    );
}
