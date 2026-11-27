#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;
use std::sync::Barrier;
use std::time::{Duration, Instant};

use barge::container::{
    ContainerClass, ContainerEndReason, ContainerEnding, ContainerLease, ContainerLeaseOutcome,
    ContainerLeaseRequest, ContainerOwner,
};
use barge::{Barge, ErrorKind};
use serde_json::json;

use common::Fixture;

fn declare(barge: &Barge, profiles: &[(&str, u32)]) {
    let profiles: Vec<_> = profiles
        .iter()
        .map(|(name, max)| json!({ "name": name, "max": max }))
        .collect();
    fs::write(
        barge.state_dir().join("container-profiles.json"),
        json!({ "profiles": profiles }).to_string(),
    )
    .unwrap();
}

fn request(project: &str, checkout: &Path) -> ContainerLeaseRequest {
    ContainerLeaseRequest {
        profile: "effigy".to_owned(),
        project: project.to_owned(),
        owner: ContainerOwner::Checkout(checkout.to_path_buf()),
        class: ContainerClass::Worker,
        hold: false,
        wait: None,
    }
}

fn lease(barge: &Barge, request: &ContainerLeaseRequest) -> ContainerLease {
    match barge.container_lease(request).unwrap() {
        ContainerLeaseOutcome::Leased(lease) => lease,
        outcome => panic!("expected a lease, got {outcome:?}"),
    }
}

#[test]
fn missing_and_unknown_profiles_are_not_managed_even_without_an_account() {
    let fixture = Fixture::new();
    let absent = fixture.temp.child("absent-account");
    let readonly =
        Barge::open_read_only_with(&absent, fixture.os.clone(), fixture.clock.clone()).unwrap();
    let requested = request("test", &fixture.home);
    assert!(matches!(
        readonly.container_lease(&requested).unwrap(),
        ContainerLeaseOutcome::NotManaged
    ));
    assert!(readonly.container_status().unwrap().is_empty());
    assert!(!absent.exists());

    declare(&fixture.barge, &[("effigy-release", 1)]);
    assert!(matches!(
        fixture.barge.container_lease(&requested).unwrap(),
        ContainerLeaseOutcome::NotManaged
    ));
}

#[test]
fn ended_workers_still_use_capacity_until_returned() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare(barge, &[("effigy", 1), ("effigy-release", 1)]);
    let checkout = fixture.temp.folder("owner").unwrap();
    let first = lease(barge, &request("first", &checkout));
    fs::remove_dir(&checkout).unwrap();
    let ended = barge.container_status().unwrap().remove(0).ended;
    assert_eq!(
        ended,
        Some(ContainerEnding {
            reason: ContainerEndReason::OwnerEnded,
            remove_volumes: true,
        })
    );

    let other_profile = ContainerLeaseRequest {
        profile: "effigy-release".to_owned(),
        ..request("first", &fixture.home)
    };
    assert_ne!(lease(barge, &other_profile).id(), first.id());

    let mut second = request("second", &fixture.home);
    assert!(matches!(
        barge.container_lease(&second).unwrap(),
        ContainerLeaseOutcome::Full { profile, max: 1, active: 1 } if profile == "effigy"
    ));
    let wait = Duration::from_millis(75);
    second.wait = Some(wait);
    let started = Instant::now();
    assert!(matches!(
        barge.container_lease(&second).unwrap(),
        ContainerLeaseOutcome::Full { .. }
    ));
    assert!(started.elapsed() >= wait);

    let begin = Barrier::new(2);
    std::thread::scope(|scope| {
        second.wait = Some(Duration::from_secs(5));
        let waiter = scope.spawn(|| {
            begin.wait();
            lease(barge, &second)
        });
        begin.wait();
        barge.container_return(first.id()).unwrap();
        assert_eq!(waiter.join().unwrap().project(), "second");
    });
    barge.container_return(first.id()).unwrap();
    assert_eq!(barge.container_status().unwrap().len(), 2);
}

#[test]
fn repeats_keep_the_same_id_and_interactive_leases_use_no_slots() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare(barge, &[("effigy", 1)]);
    let requested = request("worker", &fixture.home);
    let first = lease(barge, &requested);
    assert_eq!(lease(barge, &requested).id(), first.id());
    assert_eq!(
        barge
            .container_lease(&request("worker", &fixture.mount))
            .unwrap_err()
            .kind(),
        ErrorKind::Refused
    );

    let checkout = fixture.temp.folder("interactive").unwrap();
    let interactive = lease(
        barge,
        &ContainerLeaseRequest {
            class: ContainerClass::Interactive,
            hold: true,
            wait: Some(Duration::from_secs(5)),
            ..request("interactive", &checkout)
        },
    );
    assert!(interactive.is_held());
    drop(interactive);
    assert!(barge
        .container_status()
        .unwrap()
        .iter()
        .all(|lease| lease.ended.is_none()));
    fs::remove_dir(checkout).unwrap();
    let ended = barge.container_status().unwrap().remove(0).ended;
    assert_eq!(
        ended,
        Some(ContainerEnding {
            reason: ContainerEndReason::OwnerEnded,
            remove_volumes: true,
        })
    );

    // The ended interactive lease also uses no slot.
    barge.container_return(first.id()).unwrap();
    lease(barge, &request("second-worker", &fixture.home));
}

#[test]
fn last_hold_releases_without_releasing_the_reservation() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare(barge, &[("effigy", 1)]);
    let requested = ContainerLeaseRequest {
        hold: true,
        ..request("held", &fixture.home)
    };
    let first = lease(barge, &requested);
    let second = lease(barge, &requested);
    assert!(first.is_held() && second.is_held());
    assert_eq!(first.id(), second.id());
    assert_eq!(second.profile(), "effigy");
    drop(first);
    assert!(barge.container_status().unwrap()[0].ended.is_none());

    let id = second.id().to_owned();
    drop(second);
    assert_eq!(
        barge.container_status().unwrap()[0].ended,
        Some(ContainerEnding {
            reason: ContainerEndReason::HoldReleased,
            remove_volumes: false,
        })
    );
    assert_eq!(
        barge.container_lease(&requested).unwrap_err().kind(),
        ErrorKind::Refused
    );
    barge.container_return(&id).unwrap();
    assert_ne!(lease(barge, &requested).id(), id);
}

#[test]
fn a_missing_hold_file_leaves_the_lease_reserved() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare(barge, &[("effigy", 1)]);
    let requested = ContainerLeaseRequest {
        hold: true,
        ..request("held", &fixture.home)
    };
    let held = lease(barge, &requested);
    fs::remove_file(
        barge
            .state_dir()
            .join("container-leases")
            .join(format!("{}.lock", held.id())),
    )
    .unwrap();
    assert!(barge.container_status().unwrap()[0].ended.is_none());
    let repeated = lease(barge, &requested);
    assert_eq!(repeated.id(), held.id());
    assert!(repeated.is_held());
    assert!(matches!(
        barge
            .container_lease(&request("other", &fixture.home))
            .unwrap(),
        ContainerLeaseOutcome::Full { .. }
    ));
}

#[test]
fn missing_checkout_requires_its_registered_volume_and_parent() {
    let fixture = Fixture::new();
    fixture.register();
    let barge = &fixture.barge;
    declare(barge, &[("effigy", 1)]);
    let checkout = fixture.mount.join("checkout");
    fs::create_dir(&checkout).unwrap();
    let requested = ContainerLeaseRequest {
        hold: true,
        ..request("registered", &checkout)
    };
    let held = lease(barge, &requested);
    drop(held);
    fs::remove_dir(&checkout).unwrap();

    for identity in [None, Some(common::OTHER_UUID)] {
        fixture.os.set_uuid(&fixture.mount, identity);
        assert!(barge.container_status().unwrap()[0].ended.is_none());
    }
    fixture.os.set_uuid(&fixture.mount, Some(common::UUID));
    assert_eq!(
        barge.container_status().unwrap()[0].ended,
        Some(ContainerEnding {
            reason: ContainerEndReason::OwnerEnded,
            remove_volumes: true,
        })
    );
}

#[test]
fn missing_unregistered_checkout_parent_is_unknown() {
    let fixture = Fixture::new();
    let barge = &fixture.barge;
    declare(barge, &[("effigy", 1)]);
    let mount = fixture.temp.folder("unregistered-mount").unwrap();
    let checkout = mount.join("checkout");
    fs::create_dir(&checkout).unwrap();
    lease(barge, &request("unregistered", &checkout));
    fs::remove_dir(&checkout).unwrap();
    fs::remove_dir(&mount).unwrap();
    assert!(barge.container_status().unwrap()[0].ended.is_none());
    fs::create_dir(&mount).unwrap();
    assert_eq!(
        barge.container_status().unwrap()[0].ended,
        Some(ContainerEnding {
            reason: ContainerEndReason::OwnerEnded,
            remove_volumes: true,
        })
    );
}
