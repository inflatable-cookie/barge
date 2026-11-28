//! Read-only CLI placement preview and machine lease status.

use barge::run::{DenyReason, MachineEndReason, RunPreview, RunRequest};
use barge::{Barge, Error, Result};
use serde_json::json;

use crate::output::document;

pub(crate) fn run(arguments: &[String], json: bool) -> Result<()> {
    match arguments {
        [command] if command == "resolve" => resolve(json),
        [command] if command == "status" => status(json),
        _ => Err(Error::usage("use run resolve or run status")),
    }
}

fn resolve(json: bool) -> Result<()> {
    let request: RunRequest = serde_json::from_reader(std::io::stdin().lock())
        .map_err(|error| Error::usage(format!("cannot read run request from stdin: {error}")))?;
    let preview = Barge::open_read_only()?.run_preview(&request)?;
    if json {
        return document(
            "barge.run-resolution/1",
            vec![("resolution", json!(preview))],
        );
    }
    match preview {
        RunPreview::LocalNative => println!("local native"),
        RunPreview::LocalContainer { profile } => println!("local container profile {profile}"),
        RunPreview::Remote { machine } => println!("remote machine {machine}"),
        RunPreview::Denied { reason } => match reason {
            DenyReason::NoMatchingPlatform => println!("denied: no matching platform"),
            DenyReason::UnmetCapability => println!("denied: unmet capability"),
            DenyReason::FallbackForbidden => println!("denied: local fallback forbidden"),
            DenyReason::MachinesFull => println!("denied: matching remote machines are full"),
            DenyReason::MachinesHeld => println!("denied: matching remote machines are held"),
            DenyReason::ProfileFull {
                profile,
                active,
                max,
            } => {
                println!("denied: profile {profile} is full ({active}/{max} slots)")
            }
        },
        _ => return Err(Error::unsupported("unrecognised run resolution")),
    }
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let status = Barge::open_read_only()?.machine_status()?;
    if json {
        return document(
            "barge.run-status/1",
            vec![
                ("machine_leases", json!(status.leases)),
                ("machine_holds", json!(status.holds)),
            ],
        );
    }
    if status.leases.is_empty() && status.holds.is_empty() {
        println!("no machine leases or operator holds");
    }
    for hold in status.holds {
        match hold.reason {
            Some(reason) => println!("{} held ({reason})", hold.machine),
            None => println!("{} held", hold.machine),
        }
    }
    for lease in status.leases {
        let state = match lease.ended {
            Some(MachineEndReason::OwnerEnded) => "owner ended",
            Some(MachineEndReason::HoldReleased) => "hold released",
            None => "reserved",
        };
        println!(
            "{} slot {}: {} (lease {}, {state})",
            lease.machine, lease.slot, lease.repo, lease.id
        );
    }
    Ok(())
}
