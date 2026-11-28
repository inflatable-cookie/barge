#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::env::consts::{ARCH, OS};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use barge::run::{
    AreaUse, Capabilities, DenyReason, Fallback, LogRef, MachineEndReason, MachineLease, Platform,
    PlatformConstraint, Resolution, RunOutcome, RunOwner, RunPreview, RunRequest, RunResult,
    RunSource, Waiting,
};
use barge::ErrorKind;
use serde_json::{json, Value};

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

fn declare_remotes(fixture: &Fixture, targets: &[Value]) {
    fs::write(
        fixture.barge.state_dir().join("run-targets.json"),
        json!({
            "remote": targets,
            "local_container": [{ "profile": PROFILE, "platforms": [format!("{CONTAINER_OS}-any")] }]
        })
        .to_string(),
    )
    .unwrap();
}

fn remote(machine: &str, max: u32) -> Value {
    json!({"machine": machine, "platform": format!("{CONTAINER_OS}-amd64"), "max": max})
}

fn machine_lease(resolution: Resolution) -> MachineLease {
    let Resolution::Remote { lease } = resolution else {
        panic!("expected a machine lease, got {resolution:?}");
    };
    lease
}

#[test]
fn remote_precedes_native_and_container() {
    let fixture = Fixture::new();
    declare_remotes(&fixture, &[remote("build-pc", 1)]);
    declare_profile(&fixture);

    for platform in [
        PlatformConstraint::Any,
        PlatformConstraint::Exact {
            os: CONTAINER_OS.to_owned(),
            arch: "x86_64".to_owned(),
        },
    ] {
        let requested = request(&fixture, platform);
        assert_eq!(
            fixture.barge.run_preview(&requested).unwrap(),
            RunPreview::Remote {
                machine: "build-pc".to_owned()
            }
        );
        assert!(fixture.barge.machine_status().unwrap().leases.is_empty());
        let lease = machine_lease(resolve(&fixture, &requested));
        assert_eq!((lease.machine(), lease.slot()), ("build-pc", 0));
        assert!(lease.is_held());
        fixture.barge.machine_return(lease.id()).unwrap();
    }
    assert!(fixture.barge.container_status().unwrap().is_empty());
}

#[test]
fn warm_slots_win_over_earlier_cold_slots_and_machines() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare_remotes(&fixture, &[remote("first", 2), remote("second", 1)]);
    let requested = request(&fixture, PlatformConstraint::Any);
    let mut other = requested.clone();
    other.source.repo = "https://example.test/other.git".to_owned();

    let first = machine_lease(resolve(&fixture, &other));
    let second = machine_lease(resolve(&fixture, &requested));
    let third = machine_lease(resolve(&fixture, &other));
    assert_eq!((first.machine(), first.slot()), ("first", 0));
    assert_eq!((second.machine(), second.slot()), ("first", 1));
    assert_eq!((third.machine(), third.slot()), ("second", 0));
    for lease in [first, second, third] {
        barge.machine_return(lease.id()).unwrap();
    }

    let warm = machine_lease(resolve(&fixture, &requested));
    assert_eq!((warm.machine(), warm.slot()), ("first", 1));
    barge.machine_hold("first", None).unwrap();
    let warm_elsewhere = machine_lease(resolve(&fixture, &requested));
    assert_eq!(
        (warm_elsewhere.machine(), warm_elsewhere.slot()),
        ("second", 0)
    );
    barge.machine_return(warm_elsewhere.id()).unwrap();
    barge.machine_return(warm.id()).unwrap();
    barge.machine_release("first").unwrap();

    assert_eq!(
        barge.run_preview(&other).unwrap(),
        RunPreview::Remote {
            machine: "first".to_owned()
        }
    );
    // Free slot 0 on the first machine is cold for this repo. Occupy its
    // warm slot so the second machine's warm slot should win.
    let busy = machine_lease(resolve(&fixture, &requested));
    assert_eq!((busy.machine(), busy.slot()), ("first", 1));
    let next = machine_lease(resolve(&fixture, &requested));
    assert_eq!((next.machine(), next.slot()), ("second", 0));
}

#[test]
fn concurrent_runs_compete_for_the_same_machine_capacity() {
    let fixture = Fixture::new();
    declare_remotes(&fixture, &[remote("build-pc", 1)]);
    let mut requested = request(&fixture, PlatformConstraint::Any);
    requested.fallback = Fallback::RemoteOnly;
    let start = std::sync::Barrier::new(2);
    let attempt = || {
        start.wait();
        resolve(&fixture, &requested)
    };
    let (left, right) = std::thread::scope(|scope| {
        let first = scope.spawn(attempt);
        let second = attempt();
        (first.join().unwrap(), second)
    });
    assert!(matches!(
        (left, right),
        (
            Resolution::Remote { .. },
            Resolution::Denied {
                reason: DenyReason::MachinesFull
            }
        ) | (
            Resolution::Denied {
                reason: DenyReason::MachinesFull
            },
            Resolution::Remote { .. }
        )
    ));
    assert_eq!(fixture.barge.machine_status().unwrap().leases.len(), 1);
}

#[test]
fn a_full_machine_reports_waiting_and_reuses_a_returned_slot() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare_remotes(&fixture, &[remote("build-pc", 1)]);
    let requested = request(&fixture, PlatformConstraint::Any);
    let held = machine_lease(resolve(&fixture, &requested));
    let id = held.id().to_owned();
    drop(held);
    let mut events = Vec::new();
    let next = barge
        .run_resolve(&requested, Some(Duration::from_secs(5)), &mut |event| {
            events.push(event);
            barge.machine_return(&id).unwrap();
        })
        .unwrap();

    assert_eq!(
        events,
        [Waiting {
            machine_or_profile: "build-pc".to_owned(),
            position: 1
        }]
    );
    let next = machine_lease(next);
    assert_eq!(next.slot(), 0);
    assert_ne!(next.id(), id);
    barge.machine_return(&id).unwrap();
    assert_eq!(barge.machine_status().unwrap().leases[0].id, next.id());
}

#[test]
fn full_machines_fall_through_only_when_local_is_allowed() {
    let fixture = Fixture::new();
    declare_remotes(&fixture, &[remote("build-pc", 1)]);
    declare_profile(&fixture);
    let mut requested = request(&fixture, PlatformConstraint::Any);
    let _held = machine_lease(resolve(&fixture, &requested));
    let wait = Duration::from_millis(45);
    let started = Instant::now();
    let resolution = fixture
        .barge
        .run_resolve(&requested, Some(wait), &mut |_| {})
        .unwrap();
    assert!(started.elapsed() >= wait);
    assert!(matches!(resolution, Resolution::LocalNative));

    requested.platform = PlatformConstraint::Os(CONTAINER_OS.to_owned());
    assert!(matches!(
        resolve(&fixture, &requested),
        Resolution::LocalContainer { .. }
    ));
    requested.fallback = Fallback::RemoteOnly;
    assert!(matches!(
        resolve(&fixture, &requested),
        Resolution::Denied {
            reason: DenyReason::MachinesFull
        }
    ));
}

#[test]
fn operator_hold_skips_a_machine_and_preserves_its_lease() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare_remotes(&fixture, &[remote("first", 2), remote("second", 1)]);
    let mut requested = request(&fixture, PlatformConstraint::Any);
    requested.fallback = Fallback::RemoteOnly;
    let existing = machine_lease(resolve(&fixture, &requested));
    barge
        .machine_hold("first", Some("gaming".to_owned()))
        .unwrap();
    let status = barge.machine_status().unwrap();
    assert_eq!(status.leases[0].id, existing.id());
    assert!(status.leases[0].ended.is_none());
    assert_eq!(status.holds[0].reason.as_deref(), Some("gaming"));
    let next = machine_lease(resolve(&fixture, &requested));
    assert_eq!(next.machine(), "second");
    barge.machine_hold("second", None).unwrap();
    assert!(matches!(
        resolve(&fixture, &requested),
        Resolution::Denied {
            reason: DenyReason::MachinesHeld
        }
    ));
    barge.machine_release("first").unwrap();
    barge.machine_release("first").unwrap();
    assert_eq!(
        machine_lease(resolve(&fixture, &requested)).machine(),
        "first"
    );
}

#[test]
fn remote_capability_filter_checks_cores_memory_gpu_and_vram() {
    let fixture = Fixture::new();
    let mut target = remote("build-pc", 1);
    target["capabilities"] = json!({"cores": 16, "memory_gib": 30, "gpu": true, "vram_gib": 8});
    declare_remotes(&fixture, &[target.clone()]);
    let mut requested = request(&fixture, PlatformConstraint::Any);
    requested.fallback = Fallback::RemoteOnly;
    requested.needs.min_cores = Some(16);
    requested.needs.min_memory_bytes = Some(30 * common::GIB);
    requested.needs.gpu = true;
    requested.needs.min_vram_bytes = Some(8 * common::GIB);
    assert_eq!(
        fixture.barge.run_preview(&requested).unwrap(),
        RunPreview::Remote {
            machine: "build-pc".to_owned()
        }
    );

    for (name, value) in [
        ("cores", json!(15)),
        ("memory_gib", json!(29)),
        ("gpu", json!(false)),
        ("vram_gib", json!(7)),
    ] {
        let mut weaker = target.clone();
        weaker["capabilities"][name] = value;
        declare_remotes(&fixture, &[weaker]);
        assert!(
            matches!(
                resolve(&fixture, &requested),
                Resolution::Denied {
                    reason: DenyReason::UnmetCapability
                }
            ),
            "{name} must filter out the target"
        );
    }
}

#[test]
fn machine_status_keeps_unknown_checkouts_and_reports_known_endings() {
    let fixture = Fixture::new();
    fixture.register();
    let barge = &fixture.barge;
    declare_remotes(&fixture, &[remote("build-pc", 1)]);
    let checkout = fixture.mount.join("checkout");
    fs::create_dir(&checkout).unwrap();
    let mut requested = request(&fixture, PlatformConstraint::Any);
    requested.owner = RunOwner::Checkout(checkout.clone());
    let lease = machine_lease(resolve(&fixture, &requested));
    let id = lease.id().to_owned();
    let active = barge.machine_status().unwrap();
    assert_eq!(active.leases[0].repo, requested.source.repo);
    assert_eq!(active.leases[0].owner, requested.owner);
    assert!(active.leases[0].ended.is_none());

    drop(lease);
    assert_eq!(
        barge.machine_status().unwrap().leases[0].ended,
        Some(MachineEndReason::HoldReleased)
    );
    fs::remove_dir(&checkout).unwrap();
    for identity in [None, Some(common::OTHER_UUID)] {
        fixture.os.set_uuid(&fixture.mount, identity);
        assert!(barge.machine_status().unwrap().leases[0].ended.is_none());
    }
    fixture.os.set_uuid(&fixture.mount, Some(common::UUID));
    assert_eq!(
        barge.machine_status().unwrap().leases[0].ended,
        Some(MachineEndReason::OwnerEnded)
    );
    barge.machine_return(&id).unwrap();
    barge.machine_return(&id).unwrap();
    assert!(barge.machine_status().unwrap().leases.is_empty());
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
fn duplicate_remote_machine_ids_cannot_declare_separate_capacity() {
    let fixture = Fixture::new();
    declare_remotes(&fixture, &[remote("build-pc", 1), remote("build-pc", 2)]);
    let error = fixture
        .barge
        .run_preview(&request(&fixture, PlatformConstraint::Any))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnknownState);
    assert!(error.message().contains("declared twice"));
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
