//! Operation-level controls for the actual shared listing implementation.

// Assertion failures are test failures; shipped code keeps the crate-wide denies.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use super::{arguments, list, Listing, ScanBudget, Source, MAX_ROWS, MAX_TIME};
use crate::os::facts::ProcessIdentity;

const IDENTITY: ProcessIdentity = ProcessIdentity {
    pid: 42,
    start: 700,
    effective_uid: Some(501),
};

#[derive(Debug)]
enum Read {
    Full,
    Rows(usize),
    Entries(Vec<libc::proc_fdinfo>),
    Raw { bytes: i32, errno: i32 },
}

#[derive(Debug)]
struct Identity {
    answer: io::Result<Option<ProcessIdentity>>,
    advance: Duration,
}

impl Identity {
    fn reports(answer: Option<ProcessIdentity>) -> Self {
        Self {
            answer: Ok(answer),
            advance: Duration::ZERO,
        }
    }
}

#[derive(Debug)]
struct Kernel {
    started: Instant,
    elapsed: Duration,
    identities: VecDeque<Identity>,
    reads: VecDeque<Read>,
    allocations: Vec<usize>,
    identity_calls: usize,
    read_calls: usize,
    allocation_failure: bool,
    allocation_advance: Duration,
    read_advance: Duration,
}

impl Kernel {
    fn new(reads: impl IntoIterator<Item = Read>) -> Self {
        Self {
            started: Instant::now(),
            elapsed: Duration::ZERO,
            identities: VecDeque::new(),
            reads: reads.into_iter().collect(),
            allocations: Vec::new(),
            identity_calls: 0,
            read_calls: 0,
            allocation_failure: false,
            allocation_advance: Duration::ZERO,
            read_advance: Duration::ZERO,
        }
    }

    fn budget(&self, rows: usize) -> ScanBudget {
        ScanBudget::new(self.started, rows, MAX_TIME).expect("a bounded deadline")
    }
}

impl Source for Kernel {
    fn identity(&mut self, pid: u32) -> io::Result<Option<ProcessIdentity>> {
        assert_eq!(pid, IDENTITY.pid);
        self.identity_calls += 1;
        let Some(identity) = self.identities.pop_front() else {
            return Ok(Some(IDENTITY));
        };
        self.elapsed += identity.advance;
        identity.answer
    }

    fn read(&mut self, pid: libc::c_int, fds: &mut [libc::proc_fdinfo]) -> (i32, i32) {
        assert_eq!(pid, i32::try_from(IDENTITY.pid).expect("fixture pid fits"));
        self.read_calls += 1;
        self.elapsed += self.read_advance;
        let answer = self.reads.pop_front().expect("every read is declared");
        let rows = match answer {
            Read::Full => fds.len(),
            Read::Rows(rows) => {
                assert!(rows < fds.len(), "short answers are deliberately short");
                rows
            }
            Read::Entries(entries) => {
                assert!(entries.len() < fds.len());
                let rows = entries.len();
                fds[..rows].copy_from_slice(&entries);
                return (bytes(rows), 0);
            }
            Read::Raw { bytes, errno } => return (bytes, errno),
        };
        for (number, fd) in fds.iter_mut().take(rows).enumerate() {
            *fd = entry(i32::try_from(number).expect("bounded fixture row"), 1);
        }
        (bytes(rows), 0)
    }

    fn now(&mut self) -> Instant {
        self.started + self.elapsed
    }

    fn before_allocate(&mut self, capacity: usize) -> io::Result<()> {
        self.allocations.push(capacity);
        self.elapsed += self.allocation_advance;
        if self.allocation_failure {
            return Err(io::Error::other("injected fallible allocation refusal"));
        }
        Ok(())
    }
}

fn bytes(rows: usize) -> i32 {
    i32::try_from(rows * std::mem::size_of::<libc::proc_fdinfo>()).expect("fixture bytes fit")
}

fn entry(descriptor: i32, kind: u32) -> libc::proc_fdinfo {
    libc::proc_fdinfo {
        proc_fd: descriptor,
        proc_fdtype: kind,
    }
}

fn unreadable(listing: &Listing) -> &io::Error {
    let Listing::Unreadable(error) = listing else {
        panic!("expected unreadable, got {listing:?}");
    };
    error
}

#[test]
fn a_short_listing_preserves_every_sorted_row_and_charges_once() {
    let mut kernel = Kernel::new([Read::Entries(vec![entry(8, 900), entry(2, 1), entry(4, 0)])]);
    let mut budget = kernel.budget(100);
    let Listing::Complete {
        identity,
        fds,
        capacity,
    } = list(&mut kernel, IDENTITY, &mut budget, None)
    else {
        panic!("a short well-formed listing completes");
    };
    assert_eq!(identity, IDENTITY);
    assert_eq!(capacity, 100);
    assert_eq!(
        fds.iter()
            .map(|fd| (fd.proc_fd, fd.proc_fdtype))
            .collect::<Vec<_>>(),
        vec![(2, 1), (4, 0), (8, 900)]
    );
    assert_eq!(budget.remaining, 97);
    assert_eq!(kernel.identity_calls, 2);
    assert_eq!(kernel.read_calls, 1);
    assert_eq!(budget.attempts[0].before, IDENTITY);
    assert_eq!(budget.attempts[0].after, Some(IDENTITY));
}

#[test]
fn a_filled_prefix_grows_and_is_charged_as_well_as_the_final_listing() {
    let mut kernel = Kernel::new([Read::Full, Read::Rows(5003)]);
    let mut budget = kernel.budget(MAX_ROWS);
    let Listing::Complete { fds, capacity, .. } = list(&mut kernel, IDENTITY, &mut budget, None)
    else {
        panic!("the second complete sample replaces the full prefix");
    };
    assert_eq!(kernel.allocations, vec![4096, 8192]);
    assert_eq!(fds.len(), 5003);
    assert_eq!(capacity, 8192);
    assert_eq!(budget.remaining, MAX_ROWS - 4096 - 5003);
    assert_eq!(kernel.identity_calls, 4);
    assert_eq!(
        budget
            .attempts
            .iter()
            .map(|at| at.capacity)
            .collect::<Vec<_>>(),
        vec![4096, 8192]
    );
    assert_eq!(
        budget
            .attempts
            .iter()
            .map(|at| at.outcome)
            .collect::<Vec<_>>(),
        vec!["full", "short"]
    );
}

#[test]
fn exact_full_available_capacity_is_bounded_not_complete() {
    let mut kernel = Kernel::new([Read::Full]);
    let mut budget = kernel.budget(12);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    assert_eq!(budget.remaining, 0);
    assert_eq!(kernel.allocations, vec![12]);
    assert_eq!(kernel.identity_calls, 2);
}

#[test]
fn zero_or_previously_exhausted_budget_refuses_before_identity_allocation_or_read() {
    let mut kernel = Kernel::new([Read::Full]);
    let mut budget = kernel.budget(0);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    assert_eq!((kernel.identity_calls, kernel.read_calls), (0, 0));
    assert!(kernel.allocations.is_empty());
    budget.remaining = 1;
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    let calls = (
        kernel.identity_calls,
        kernel.read_calls,
        kernel.allocations.len(),
    );
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    assert_eq!(
        (
            kernel.identity_calls,
            kernel.read_calls,
            kernel.allocations.len()
        ),
        calls
    );
}

#[test]
fn no_growth_sequence_can_spend_more_than_the_original_rows_or_read_five_times() {
    let mut kernel = Kernel::new([Read::Full, Read::Full, Read::Full, Read::Full]);
    let mut budget = kernel.budget(usize::MAX);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    assert_eq!(kernel.allocations, vec![4096, 8192, 16384, 32768]);
    assert_eq!(kernel.read_calls, 4);
    assert_eq!(budget.remaining, 4096);
    assert_eq!(
        budget
            .attempts
            .iter()
            .map(|at| at.rows.expect("valid full answer"))
            .sum::<usize>(),
        61_440
    );
}

#[test]
fn growth_is_clamped_to_remaining_work_only_when_strictly_larger() {
    let mut kernel = Kernel::new([Read::Full, Read::Rows(4000)]);
    let mut budget = kernel.budget(10_000);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Complete { capacity: 5904, .. }
    ));
    assert_eq!(kernel.allocations, vec![4096, 5904]);
    assert_eq!(budget.remaining, 1904);
    let mut kernel = Kernel::new([Read::Full]);
    let mut budget = kernel.budget(8192);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    assert_eq!(kernel.read_calls, 1);
    assert_eq!(budget.remaining, 4096);
}

#[test]
fn repeated_and_non_vnode_rows_share_one_unrefunded_budget() {
    let mut kernel = Kernel::new([Read::Rows(3), Read::Rows(3), Read::Full]);
    let mut budget = kernel.budget(8);
    for remaining in [5, 2] {
        assert!(matches!(
            list(&mut kernel, IDENTITY, &mut budget, None),
            Listing::Complete { .. }
        ));
        assert_eq!(budget.remaining, remaining);
    }
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("descriptor work")
    ));
    assert_eq!(budget.remaining, 0);
    assert_eq!(kernel.allocations, vec![8, 5, 2]);
}

#[test]
fn malformed_byte_counts_and_negative_without_errno_never_mean_empty() {
    for invalid in [-1, 1, bytes(4) + 1, bytes(9)] {
        let mut kernel = Kernel::new([Read::Raw {
            bytes: invalid,
            errno: 0,
        }]);
        let mut budget = kernel.budget(8);
        let result = list(&mut kernel, IDENTITY, &mut budget, None);
        assert!(
            unreadable(&result).to_string().contains("byte count"),
            "{invalid}: {result:?}"
        );
        assert_eq!(kernel.read_calls, 1);
        assert_eq!(
            kernel.identity_calls, 2,
            "even invalid answers have an identity bracket"
        );
    }
}

#[test]
fn empty_requires_clear_errno_and_the_final_identity() {
    let mut kernel = Kernel::new([Read::Rows(0)]);
    let mut budget = kernel.budget(1);
    assert!(
        matches!(list(&mut kernel, IDENTITY, &mut budget, None), Listing::Complete { fds, .. } if fds.is_empty())
    );
    assert_eq!(kernel.identity_calls, 2);
    assert_eq!(budget.remaining, 1);
    for errno in [libc::EPERM, libc::ENOENT, libc::EIO] {
        let mut kernel = Kernel::new([Read::Raw { bytes: 0, errno }]);
        let mut budget = kernel.budget(1);
        assert_eq!(
            unreadable(&list(&mut kernel, IDENTITY, &mut budget, None)).raw_os_error(),
            Some(errno)
        );
        assert_eq!(kernel.identity_calls, 2);
    }
}

#[test]
fn a_positive_answer_with_any_errno_is_ambiguous_even_esrch() {
    for errno in [libc::ESRCH, libc::EPERM, libc::EIO] {
        let mut kernel = Kernel::new([Read::Raw {
            bytes: bytes(1),
            errno,
        }]);
        kernel.identities = [Identity::reports(Some(IDENTITY)), Identity::reports(None)].into();
        let mut budget = kernel.budget(5);
        unreadable(&list(&mut kernel, IDENTITY, &mut budget, None));
        assert_eq!(budget.remaining, 4);
        assert_eq!(kernel.identity_calls, 2);
        assert_eq!(
            kernel.identities.len(),
            0,
            "the observed gone identity cannot erase ambiguous bytes"
        );
    }
}

#[test]
fn negative_and_duplicate_descriptors_refuse_before_a_later_gone_fact() {
    for entries in [vec![entry(-1, 1)], vec![entry(4, 1), entry(4, 900)]] {
        let count = entries.len();
        let mut kernel = Kernel::new([Read::Entries(entries)]);
        kernel.identities = [Identity::reports(Some(IDENTITY)), Identity::reports(None)].into();
        let mut budget = kernel.budget(10);
        assert!(unreadable(&list(&mut kernel, IDENTITY, &mut budget, None))
            .to_string()
            .contains("negative or duplicate"));
        assert_eq!(budget.remaining, 10 - count);
        assert_eq!(kernel.identity_calls, 2);
    }
}

#[test]
fn explicit_esrch_settles_gone_but_other_errors_are_not_erased() {
    for bytes in [-1, 0] {
        let mut kernel = Kernel::new([Read::Raw {
            bytes,
            errno: libc::ESRCH,
        }]);
        let mut budget = kernel.budget(10);
        assert!(matches!(
            list(&mut kernel, IDENTITY, &mut budget, None),
            Listing::Gone {
                errno: Some(libc::ESRCH)
            }
        ));
    }
    for errno in [libc::EPERM, libc::ENOENT, libc::EIO] {
        let mut kernel = Kernel::new([Read::Raw { bytes: -1, errno }]);
        kernel.identities = [Identity::reports(Some(IDENTITY)), Identity::reports(None)].into();
        let mut budget = kernel.budget(10);
        assert_eq!(
            unreadable(&list(&mut kernel, IDENTITY, &mut budget, None)).raw_os_error(),
            Some(errno)
        );
        assert_eq!(kernel.identity_calls, 2);
    }
}

#[test]
fn a_failed_attempt_keeps_its_error_despite_gone_changed_or_unreadable_final_identity() {
    for after in [
        Ok(None),
        Ok(Some(ProcessIdentity {
            effective_uid: Some(502),
            ..IDENTITY
        })),
        Err(io::Error::from_raw_os_error(libc::EACCES)),
    ] {
        let mut kernel = Kernel::new([Read::Raw {
            bytes: -1,
            errno: libc::EPERM,
        }]);
        kernel.identities = [
            Identity::reports(Some(IDENTITY)),
            Identity {
                answer: after,
                advance: Duration::ZERO,
            },
        ]
        .into();
        let mut budget = kernel.budget(10);
        assert_eq!(
            unreadable(&list(&mut kernel, IDENTITY, &mut budget, None)).raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(kernel.identity_calls, 2);
        assert!(kernel.identities.is_empty());
    }
}

#[test]
fn a_late_failed_or_malformed_read_keeps_its_failure_instead_of_a_time_answer() {
    for answer in [
        Read::Raw {
            bytes: -1,
            errno: libc::EIO,
        },
        Read::Raw { bytes: 1, errno: 0 },
    ] {
        let mut kernel = Kernel::new([answer]);
        kernel.read_advance = MAX_TIME;
        let mut budget = kernel.budget(10);
        unreadable(&list(&mut kernel, IDENTITY, &mut budget, None));
        // The read consumed the original deadline: its failure survives,
        // and another identity read is not admitted after that deadline.
        assert_eq!(kernel.identity_calls, 1);
    }
}

#[test]
fn a_positive_gone_identity_before_or_after_valid_bytes_is_gone() {
    let mut kernel = Kernel::new([]);
    kernel.identities.push_back(Identity::reports(None));
    let mut budget = kernel.budget(10);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Gone { errno: None }
    ));
    assert!(kernel.allocations.is_empty());
    let mut kernel = Kernel::new([Read::Rows(3)]);
    kernel.identities = [Identity::reports(Some(IDENTITY)), Identity::reports(None)].into();
    let mut budget = kernel.budget(10);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Gone { errno: None }
    ));
    assert_eq!(budget.remaining, 7);
}

#[test]
fn stale_pid_start_uid_and_unreadable_uid_refuse_before_reading() {
    for changed in [
        ProcessIdentity {
            pid: 43,
            ..IDENTITY
        },
        ProcessIdentity {
            start: 701,
            ..IDENTITY
        },
        ProcessIdentity {
            effective_uid: Some(502),
            ..IDENTITY
        },
        ProcessIdentity {
            effective_uid: None,
            ..IDENTITY
        },
    ] {
        let mut kernel = Kernel::new([]);
        kernel
            .identities
            .push_back(Identity::reports(Some(changed)));
        let mut budget = kernel.budget(10);
        unreadable(&list(&mut kernel, IDENTITY, &mut budget, None));
        assert_eq!(kernel.read_calls, 0);
        assert!(kernel.allocations.is_empty());
    }
}

#[test]
fn identity_and_uid_must_remain_bound_after_each_read_and_before_growth() {
    for changed in [
        ProcessIdentity {
            start: 701,
            ..IDENTITY
        },
        ProcessIdentity {
            effective_uid: Some(502),
            ..IDENTITY
        },
        ProcessIdentity {
            effective_uid: None,
            ..IDENTITY
        },
    ] {
        for growth in [false, true] {
            let mut kernel = Kernel::new([if growth { Read::Full } else { Read::Rows(0) }]);
            kernel.identities = [
                Identity::reports(Some(IDENTITY)),
                Identity::reports(Some(changed)),
            ]
            .into();
            let mut budget = kernel.budget(MAX_ROWS);
            unreadable(&list(&mut kernel, IDENTITY, &mut budget, None));
            assert_eq!(kernel.read_calls, 1);
            assert_eq!(kernel.allocations, vec![4096]);
        }
    }
    let mut kernel = Kernel::new([Read::Full]);
    kernel.identities = [
        Identity::reports(Some(IDENTITY)),
        Identity::reports(Some(IDENTITY)),
        Identity::reports(Some(ProcessIdentity {
            start: 701,
            ..IDENTITY
        })),
    ]
    .into();
    let mut budget = kernel.budget(MAX_ROWS);
    unreadable(&list(&mut kernel, IDENTITY, &mut budget, None));
    assert_eq!(
        kernel.read_calls, 1,
        "growth rebinds before its allocation and read"
    );
}

#[test]
fn unreadable_identity_refuses_on_either_side_of_a_read() {
    for after in [false, true] {
        let mut kernel = Kernel::new([Read::Rows(0)]);
        if after {
            kernel
                .identities
                .push_back(Identity::reports(Some(IDENTITY)));
        }
        kernel.identities.push_back(Identity {
            answer: Err(io::Error::from_raw_os_error(libc::EIO)),
            advance: Duration::ZERO,
        });
        let mut budget = kernel.budget(10);
        assert_eq!(
            unreadable(&list(&mut kernel, IDENTITY, &mut budget, None)).raw_os_error(),
            Some(libc::EIO)
        );
        assert_eq!(kernel.read_calls, usize::from(after));
    }
}

#[test]
fn allocation_failure_is_recoverable_and_no_read_occurs() {
    let mut kernel = Kernel::new([]);
    kernel.allocation_failure = true;
    let mut budget = kernel.budget(10);
    assert!(unreadable(&list(&mut kernel, IDENTITY, &mut budget, None))
        .to_string()
        .contains("allocation"));
    assert_eq!(kernel.read_calls, 0);
    assert_eq!(budget.remaining, 10);
}

#[test]
fn exact_pid_and_kernel_byte_conversion_refuses_instead_of_substituting() {
    assert_eq!(arguments(42, 1), Some((42, bytes(1))));
    for (pid, capacity) in [
        (0, 1),
        (42, 0),
        (u32::MAX, 1),
        (42, usize::MAX),
        (42, i32::MAX as usize),
    ] {
        assert!(
            arguments(pid, capacity).is_none(),
            "pid={pid} capacity={capacity}"
        );
    }
    let mut kernel = Kernel::new([]);
    let mut budget = kernel.budget(10);
    unreadable(&list(
        &mut kernel,
        ProcessIdentity {
            pid: u32::MAX,
            ..IDENTITY
        },
        &mut budget,
        None,
    ));
    assert_eq!(kernel.identity_calls, 0);
}

#[test]
fn request_totals_are_clamped_and_smaller_deadlines_stay_smaller() {
    let started = Instant::now();
    let large = ScanBudget::new(started, usize::MAX, Duration::MAX).expect("clamped deadline fits");
    assert_eq!(large.remaining, MAX_ROWS);
    assert_eq!(large.timeout, MAX_TIME);
    let small = ScanBudget::new(started, 7, Duration::from_millis(3)).expect("small deadline fits");
    assert_eq!(small.remaining, 7);
    assert_eq!(small.timeout, Duration::from_millis(3));
}

#[test]
fn the_original_deadline_is_checked_after_allocation_read_and_final_identity() {
    for operation in ["allocate", "read", "final identity"] {
        let mut kernel = Kernel::new([Read::Rows(0)]);
        match operation {
            "allocate" => kernel.allocation_advance = MAX_TIME,
            "read" => kernel.read_advance = MAX_TIME,
            _ => {
                kernel.identities = [
                    Identity::reports(Some(IDENTITY)),
                    Identity {
                        answer: Ok(Some(IDENTITY)),
                        advance: MAX_TIME,
                    },
                ]
                .into()
            }
        }
        let mut budget = kernel.budget(10);
        assert!(
            matches!(
                list(&mut kernel, IDENTITY, &mut budget, None),
                Listing::Bounded("time")
            ),
            "{operation}"
        );
        assert_eq!(kernel.read_calls, usize::from(operation != "allocate"));
    }
}

#[test]
fn an_empty_listing_and_the_last_listing_cannot_complete_after_the_original_deadline() {
    let mut kernel = Kernel::new([Read::Rows(1), Read::Rows(0)]);
    let mut budget =
        ScanBudget::new(kernel.started, 10, Duration::from_millis(5)).expect("small deadline");
    kernel.read_advance = Duration::from_millis(2);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Complete { .. }
    ));
    kernel.read_advance = Duration::from_millis(3);
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("time")
    ));
    assert_eq!(budget.remaining, 9);
}

#[test]
fn a_growth_read_and_every_local_window_share_the_original_deadlines() {
    let mut kernel = Kernel::new([Read::Full, Read::Rows(1)]);
    kernel.read_advance = Duration::from_millis(3);
    let mut budget = ScanBudget::new(kernel.started, MAX_ROWS, Duration::from_millis(5))
        .expect("small deadline");
    assert!(matches!(
        list(&mut kernel, IDENTITY, &mut budget, None),
        Listing::Bounded("time")
    ));
    assert_eq!(budget.remaining, MAX_ROWS - 4096 - 1);
    for operation in ["allocate", "read", "final identity"] {
        let mut kernel = Kernel::new([Read::Rows(0)]);
        let pause = Duration::from_millis(1);
        match operation {
            "allocate" => kernel.allocation_advance = pause,
            "read" => kernel.read_advance = pause,
            _ => {
                kernel.identities = [
                    Identity::reports(Some(IDENTITY)),
                    Identity {
                        answer: Ok(Some(IDENTITY)),
                        advance: pause,
                    },
                ]
                .into()
            }
        }
        let local = kernel.started + pause;
        let mut budget = kernel.budget(10);
        assert!(
            unreadable(&list(&mut kernel, IDENTITY, &mut budget, Some(local)))
                .to_string()
                .contains("settle window")
        );
    }
}

#[test]
fn an_already_closed_scan_or_local_window_refuses_before_another_operation() {
    for local in [false, true] {
        let mut kernel = Kernel::new([]);
        let mut budget = if local {
            kernel.budget(10)
        } else {
            ScanBudget::new(kernel.started, 10, Duration::ZERO).expect("zero timeout")
        };
        let deadline = local.then_some(kernel.started);
        let result = list(&mut kernel, IDENTITY, &mut budget, deadline);
        if local {
            unreadable(&result);
        } else {
            assert!(matches!(result, Listing::Bounded("time")));
        }
        assert_eq!((kernel.identity_calls, kernel.read_calls), (0, 0));
        assert!(kernel.allocations.is_empty());
    }
}

#[test]
fn the_actual_fallible_allocator_refuses_capacity_overflow_without_panicking() {
    assert!(super::allocate(usize::MAX).is_err());
}
