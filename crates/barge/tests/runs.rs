#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::env::consts::{ARCH, OS};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use barge::run::{
    AreaUse, Capabilities, DenyReason, Fallback, LogRef, Platform, PlatformConstraint, Resolution,
    RunOutcome, RunOwner, RunRequest, RunResult, RunSource, Waiting,
};
use barge::ErrorKind;
use serde_json::json;

use common::Fixture;

const PROFILE: &str = "test-container";
const CONTAINER_OS: &str = if cfg!(target_os = "linux") {
    "macos"
} else {
    "linux"
};

fn request(fixture: &Fixture, platform: PlatformConstraint) -> RunRequest {
    RunRequest {
        platform,
        needs: Capabilities::default(),
        source: RunSource {
            repo: "https://example.test/project.git".to_owned(),
            base: "base".to_owned(),
            tree: "tree".to_owned(),
            bundle: Some(PathBuf::from("source.bundle")),
            patch: None,
        },
        command: vec!["check".to_owned()],
        env: Default::default(),
        budget: Duration::from_secs(30),
        fallback: Fallback::LocalAllowed,
        owner: RunOwner::Checkout(fixture.home.clone()),
    }
}

fn declare_target(fixture: &Fixture, platforms: &[String]) {
    let state = fixture.barge.state_dir();
    fs::write(
        state.join("run-targets.json"),
        json!({ "local_container": [{ "profile": PROFILE, "platforms": platforms }] }).to_string(),
    )
    .unwrap();
}

fn declare_profile(fixture: &Fixture) {
    fs::write(
        fixture.barge.state_dir().join("container-profiles.json"),
        json!({ "profiles": [{ "name": PROFILE, "max": 1 }] }).to_string(),
    )
    .unwrap();
}

fn resolve(fixture: &Fixture, request: &RunRequest) -> Resolution {
    fixture
        .barge
        .run_resolve(request, None, &mut |event| {
            panic!("unexpected wait: {event:?}")
        })
        .unwrap()
}

#[test]
fn native_matches_exact_os_and_any_without_a_targets_file() {
    let fixture = Fixture::new();
    for platform in [
        PlatformConstraint::Exact {
            os: OS.to_owned(),
            arch: ARCH.to_owned(),
        },
        PlatformConstraint::Os(OS.to_owned()),
        PlatformConstraint::Any,
    ] {
        let mut requested = request(&fixture, platform);
        requested.needs.min_cores = Some(1);
        requested.needs.min_memory_bytes = Some(1);
        assert!(matches!(
            resolve(&fixture, &requested),
            Resolution::LocalNative
        ));
    }
    for platform in [
        PlatformConstraint::Exact {
            os: OS.to_owned(),
            arch: "other_arch".to_owned(),
        },
        PlatformConstraint::Os(CONTAINER_OS.to_owned()),
    ] {
        assert!(matches!(
            resolve(&fixture, &request(&fixture, platform)),
            Resolution::Denied {
                reason: DenyReason::NoMatchingPlatform
            }
        ));
    }
    assert!(fixture.barge.container_status().unwrap().is_empty());
}

#[test]
fn native_denies_unmet_core_memory_gpu_and_vram_needs() {
    let fixture = Fixture::new();
    for set_need in [
        (|needs: &mut Capabilities| needs.min_cores = Some(u32::MAX)) as fn(&mut Capabilities),
        |needs| needs.min_memory_bytes = Some(u64::MAX),
        |needs| needs.gpu = true,
        |needs| needs.min_vram_bytes = Some(1),
    ] {
        let mut requested = request(&fixture, PlatformConstraint::Any);
        set_need(&mut requested.needs);
        assert!(
            matches!(
                resolve(&fixture, &requested),
                Resolution::Denied {
                    reason: DenyReason::UnmetCapability
                }
            ),
            "{:?}",
            requested.needs
        );
    }
}

#[test]
fn remote_only_denies_even_a_native_match() {
    let fixture = Fixture::new();
    let mut requested = request(&fixture, PlatformConstraint::Any);
    requested.fallback = Fallback::RemoteOnly;
    assert!(matches!(
        resolve(&fixture, &requested),
        Resolution::Denied {
            reason: DenyReason::FallbackForbidden
        }
    ));
}

#[test]
fn container_takes_a_worker_lease_for_each_run_and_reports_a_full_profile() {
    let fixture = Fixture::new();
    declare_target(&fixture, &[format!("{CONTAINER_OS}-arm64")]);
    declare_profile(&fixture);
    let requested = request(
        &fixture,
        PlatformConstraint::Exact {
            os: CONTAINER_OS.to_owned(),
            arch: "aarch64".to_owned(),
        },
    );
    let Resolution::LocalContainer { lease } = resolve(&fixture, &requested) else {
        panic!("expected a container lease");
    };
    assert!(lease.is_held());
    assert_eq!(lease.profile(), PROFILE);
    assert_eq!(
        fixture.barge.container_status().unwrap()[0].owner,
        requested.owner
    );

    let wait = Duration::from_millis(45);
    let started = Instant::now();
    let mut events = Vec::new();
    let resolution = fixture
        .barge
        .run_resolve(&requested, Some(wait), &mut |event| events.push(event))
        .unwrap();
    assert!(matches!(resolution,
        Resolution::Denied { reason: DenyReason::ProfileFull { profile, .. } }
        if profile == PROFILE
    ));
    assert!(started.elapsed() >= wait);
    assert_eq!(
        events,
        [Waiting {
            machine_or_profile: PROFILE.to_owned(),
            position: 1
        }]
    );

    let mut returned = false;
    let next = fixture
        .barge
        .run_resolve(&requested, Some(Duration::from_secs(5)), &mut |_| {
            fixture.barge.container_return(lease.id()).unwrap();
            returned = true;
        })
        .unwrap();
    let Resolution::LocalContainer { lease: next } = next else {
        panic!("expected a lease after the slot was returned");
    };
    assert!(returned);
    assert_ne!(next.id(), lease.id());
    assert_ne!(next.project(), lease.project());
    assert!(next.is_held());
    fixture.barge.container_return(next.id()).unwrap();
    assert!(fixture.barge.container_status().unwrap().is_empty());
}

#[test]
fn container_leases_accept_cpu_and_memory_minimums() {
    let fixture = Fixture::new();
    declare_target(&fixture, &[format!("{CONTAINER_OS}-any")]);
    declare_profile(&fixture);
    let mut requested = request(&fixture, PlatformConstraint::Os(CONTAINER_OS.to_owned()));
    requested.needs.min_cores = Some(1);
    requested.needs.min_memory_bytes = Some(common::GIB);

    let Resolution::LocalContainer { lease } = resolve(&fixture, &requested) else {
        panic!("expected a container lease for a request with CPU and memory minimums");
    };
    assert_eq!(lease.profile(), PROFILE);
    assert!(lease.is_held());
    fixture.barge.container_return(lease.id()).unwrap();
}

#[test]
fn os_only_container_target_needs_a_managed_profile_and_proves_no_architecture() {
    let fixture = Fixture::new();
    declare_target(&fixture, &[format!("{CONTAINER_OS}-any")]);
    let mut requested = request(&fixture, PlatformConstraint::Os(CONTAINER_OS.to_owned()));
    assert!(matches!(
        resolve(&fixture, &requested),
        Resolution::Denied {
            reason: DenyReason::NoMatchingPlatform
        }
    ));
    declare_profile(&fixture);
    let Resolution::LocalContainer { lease } = resolve(&fixture, &requested) else {
        panic!("expected a lease for an OS-only request");
    };
    fixture.barge.container_return(lease.id()).unwrap();
    for (gpu, min_vram_bytes) in [(true, None), (false, Some(1))] {
        requested.needs.gpu = gpu;
        requested.needs.min_vram_bytes = min_vram_bytes;
        assert!(matches!(
            resolve(&fixture, &requested),
            Resolution::Denied {
                reason: DenyReason::UnmetCapability
            }
        ));
    }
    requested.platform = PlatformConstraint::Exact {
        os: CONTAINER_OS.to_owned(),
        arch: "aarch64".to_owned(),
    };
    requested.needs = Capabilities::default();
    assert!(matches!(
        resolve(&fixture, &requested),
        Resolution::Denied {
            reason: DenyReason::NoMatchingPlatform
        }
    ));
}

#[test]
fn malformed_targets_fail_closed_when_they_are_needed() {
    let fixture = Fixture::new();
    declare_target(&fixture, &["linux".to_owned()]);
    let requested = request(&fixture, PlatformConstraint::Os(CONTAINER_OS.to_owned()));
    let error = fixture
        .barge
        .run_resolve(&requested, None, &mut |_| {})
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnknownState);
    assert!(error.message().contains("run-targets.json"));
}

#[test]
fn local_result_round_trips_as_a_signalled_run() {
    let result = RunResult {
        machine: "local".to_owned(),
        platform: Platform {
            os: OS.to_owned(),
            arch: ARCH.to_owned(),
        },
        repo: "https://example.test/project.git".to_owned(),
        base: "base".to_owned(),
        patch_digest: Some("patch-digest".to_owned()),
        tree: "tree".to_owned(),
        toolchain: None,
        command: vec!["check".to_owned()],
        started_ms: 100,
        finished_ms: 200,
        waking_ms: None,
        outcome: RunOutcome::Signalled { signal: 15 },
        log: LogRef {
            digest: "log-digest".to_owned(),
            location: "/tmp/run.log".to_owned(),
        },
        area: Some(AreaUse {
            id: "area".to_owned(),
            bytes: 2048,
        }),
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(
        json["outcome"],
        serde_json::json!({ "kind": "signalled", "signal": 15 })
    );
    assert_eq!(serde_json::from_value::<RunResult>(json).unwrap(), result);
}
