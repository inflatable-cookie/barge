//! Read-only observations for native apparatus, never custody authorization.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::custody::{SCAN_DESCRIPTORS, SCAN_PROCESSES, SCAN_TIMEOUT};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    BlindSpotEvidence, DescriptorScan, DescriptorScanRequest, DirOps, ProcessObserver,
    RetryEvidence, SelfExclusion,
};

/// The result of observing descriptor capability, not an idleness verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CensusStatus {
    /// The full product observation completed within its bounds.
    Passed,
    /// The observation was unreadable or bounded and proves nothing.
    Infrastructure,
}

/// A full same-effective-uid census through the product observer and bounds.
/// Counts absent from an unreadable product answer remain unknown (`null`).
#[derive(Debug, Serialize)]
pub struct DescriptorCensus {
    /// Whether observation completed, independently of any open writers.
    pub status: CensusStatus,
    /// The existing directory observed. It is opened but never written.
    pub root: PathBuf,
    /// The observing Barge process, excluded only by this pid/start identity.
    pub observer_pid: u32,
    /// The observer's kernel start identity.
    pub observer_start: u64,
    /// The account whose processes are observed.
    pub effective_uid: u32,
    /// The production maximum same-uid process count.
    pub process_limit: usize,
    /// The production total descriptor limit.
    pub descriptor_limit: usize,
    /// The production scan deadline in milliseconds.
    pub timeout_ms: u128,
    /// The process count, when supplied by the product answer.
    pub processes: Option<usize>,
    /// The descriptor count, when supplied by the product answer.
    pub descriptors: Option<usize>,
    /// Foreign effective uids excluded by the product observer.
    pub foreign: Option<usize>,
    /// Open descriptors under the root; informational, never custody authority.
    pub open: Option<usize>,
    /// Why an incomplete observation is infrastructure.
    pub problem: Option<String>,
    /// The unchanged bounded re-observation evidence from the product scan.
    pub retry: RetryEvidence,
    /// Platform-verified opaque descriptors, when supplied by a complete scan.
    /// Incomplete scans do not supply this evidence and report `null`.
    pub os_blind_spot: Option<BlindSpotEvidence>,
}

/// Observe the full same-uid descriptor set with the exact custody observer.
///
/// This takes no locks, registers nothing and writes nothing. Foreign effective
/// uids and positively exited processes follow the product policy. An unreadable
/// same-uid listing or unexplained vnode path, or any bound, is infrastructure.
/// Verified platform blind spots follow the product policy and stay visible.
/// A completed
/// observation can report open descriptors: it is never a `Safe` custody value.
pub fn descriptor_census(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    root: &Path,
) -> Result<DescriptorCensus> {
    if !root.is_absolute() {
        return Err(Error::new(
            ErrorKind::Usage,
            "the census root must be absolute",
        ));
    }
    let opened = dirs.open_path(root)?;
    let observer_pid = std::process::id();
    let observer_start = observer.process_start(observer_pid)?.ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "the census observer has no start identity",
        )
    })?;
    let effective_uid = observer.effective_uid()?;
    let scan = observer.scan_open_descriptors(&DescriptorScanRequest {
        root,
        root_device: opened.identity().device,
        root_inode: opened.identity().inode,
        max_processes: SCAN_PROCESSES,
        max_descriptors: SCAN_DESCRIPTORS,
        timeout: SCAN_TIMEOUT,
        self_exclusion: Some(SelfExclusion {
            pid: observer_pid,
            start: observer_start,
        }),
    })?;
    let mut report = DescriptorCensus {
        status: CensusStatus::Infrastructure,
        root: root.to_path_buf(),
        observer_pid,
        observer_start,
        effective_uid,
        process_limit: SCAN_PROCESSES,
        descriptor_limit: SCAN_DESCRIPTORS,
        timeout_ms: SCAN_TIMEOUT.as_millis(),
        processes: None,
        descriptors: None,
        foreign: None,
        open: None,
        problem: None,
        retry: RetryEvidence::default(),
        os_blind_spot: None,
    };
    match scan {
        DescriptorScan::Complete {
            processes,
            descriptors,
            foreign,
            retry,
            open,
            os_blind_spot,
        } => {
            report.status = CensusStatus::Passed;
            report.processes = Some(processes);
            report.descriptors = Some(descriptors);
            report.foreign = Some(foreign);
            report.open = Some(open.len());
            report.retry = retry;
            report.os_blind_spot = Some(os_blind_spot);
        }
        DescriptorScan::Bounded {
            processes,
            descriptors,
            bound,
            retry,
        } => {
            report.processes = Some(processes);
            report.descriptors = Some(descriptors);
            report.problem = Some(format!(
                "the product descriptor scan reached its {bound} bound"
            ));
            report.retry = retry;
        }
        DescriptorScan::Unreadable { reason, retry } => {
            report.problem = Some(reason);
            report.retry = retry;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::os::{platform, RetriedUnreadable, RetryResolution};
    use crate::testkit::TempDir;

    struct Observation(DescriptorScan);

    impl ProcessObserver for Observation {
        fn effective_uid(&self) -> Result<u32> {
            Ok(501)
        }
        fn boot_identity(&self) -> Result<String> {
            Ok("fixture".into())
        }
        fn process_start(&self, pid: u32) -> Result<Option<u64>> {
            assert_eq!(pid, std::process::id());
            Ok(Some(123))
        }
        fn verify_platform_binary(
            &self,
            _: crate::os::ProcessIdentity,
        ) -> Result<crate::os::PlatformStatus> {
            panic!("the diagnostic must delegate platform classification to the product observer")
        }
        fn scan_open_descriptors(
            &self,
            request: &DescriptorScanRequest<'_>,
        ) -> Result<DescriptorScan> {
            assert_eq!(request.max_processes, SCAN_PROCESSES);
            assert_eq!(request.max_descriptors, SCAN_DESCRIPTORS);
            assert_eq!(request.timeout, SCAN_TIMEOUT);
            assert_eq!(
                request.self_exclusion,
                Some(SelfExclusion {
                    pid: std::process::id(),
                    start: 123
                })
            );
            assert!(request.root.is_absolute());
            assert_eq!(
                request.root_inode,
                std::fs::metadata(request.root).unwrap().ino()
            );
            Ok(self.0.clone())
        }
    }

    use std::os::unix::fs::MetadataExt;

    #[test]
    fn a_complete_product_census_preserves_foreign_and_gone_evidence() {
        let fixture = TempDir::new("census-complete");
        let retry = RetryEvidence {
            retried_unreadable: 1,
            retried: vec![RetriedUnreadable {
                pid: 42,
                command: Some("exited-fixture".into()),
                start: 1,
                errnos: vec![1, 3],
                identity_changes: 0,
                resolution: RetryResolution::Gone,
            }],
        };
        let report = descriptor_census(
            &platform(),
            &Observation(DescriptorScan::Complete {
                processes: 2,
                descriptors: 5,
                foreign: 3,
                retry: retry.clone(),
                os_blind_spot: BlindSpotEvidence {
                    processes: 1,
                    opaque_descriptors: 2,
                    records: vec![crate::os::PlatformBlindSpot {
                        pid: 43,
                        start: 321,
                        command: "platform-fixture".into(),
                        opaque_descriptors: 2,
                    }],
                },
                open: vec![],
            }),
            fixture.path(),
        )
        .unwrap();
        assert_eq!(report.status, CensusStatus::Passed);
        assert_eq!(report.foreign, Some(3));
        assert_eq!(report.processes, Some(2));
        assert_eq!(report.descriptors, Some(5));
        assert_eq!(report.retry, retry);
        let blind_spot = report.os_blind_spot.as_ref().unwrap();
        assert_eq!(blind_spot.processes, 1);
        assert_eq!(blind_spot.opaque_descriptors, 2);
        assert_eq!(blind_spot.records[0].command, "platform-fixture");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["os_blind_spot"]["records"][0]["pid"], 43);
        assert!(std::fs::read_dir(fixture.path()).unwrap().next().is_none());
    }

    #[test]
    fn unreadable_and_bounded_product_answers_are_infrastructure_with_retry_evidence() {
        let fixture = TempDir::new("census-refused");
        let retry = RetryEvidence {
            retried_unreadable: 1,
            retried: vec![RetriedUnreadable {
                pid: 42,
                command: Some("unreadable-fixture".into()),
                start: 1,
                errnos: vec![1, 1, 1],
                identity_changes: 0,
                resolution: RetryResolution::Unreadable,
            }],
        };
        for scan in [
            DescriptorScan::Unreadable {
                reason: "pid 42 remains unreadable".into(),
                retry: retry.clone(),
            },
            DescriptorScan::Bounded {
                processes: 2,
                descriptors: 5,
                bound: "time",
                retry: retry.clone(),
            },
        ] {
            let report =
                descriptor_census(&platform(), &Observation(scan), fixture.path()).unwrap();
            assert_eq!(report.status, CensusStatus::Infrastructure);
            assert!(report.problem.is_some());
            assert_eq!(report.retry, retry);
            assert!(
                report.os_blind_spot.is_none(),
                "missing evidence is never an empty census"
            );
        }
        assert!(std::fs::read_dir(fixture.path()).unwrap().next().is_none());
    }
}
