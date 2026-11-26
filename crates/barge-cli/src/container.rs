//! CLI inspection and explicit return for container reservations.

use barge::container::{ContainerClass, ContainerEndReason, ContainerOwner};
use barge::{Barge, Error, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::output::document;

pub(crate) fn container(arguments: &[String], json: bool) -> Result<()> {
    let Some((command, rest)) = arguments.split_first() else {
        return Err(Error::usage("container needs status or return"));
    };
    match command.as_str() {
        "status" => status(rest, json),
        "return" => give_back(rest, json),
        other => Err(Error::usage(format!("unknown container command {other:?}"))),
    }
}

fn status(arguments: &[String], json: bool) -> Result<()> {
    expect_no_positionals(&Options::parse(arguments, &[], &[])?)?;
    let leases = Barge::open_read_only()?.container_status()?;
    if json {
        return document("barge.container-status/1", vec![("leases", json!(leases))]);
    }
    if leases.is_empty() {
        println!("no container leases");
    }
    for lease in leases {
        let class = match lease.class {
            ContainerClass::Worker => "worker",
            ContainerClass::Interactive => "interactive",
        };
        let owner = match &lease.owner {
            ContainerOwner::Worktree(id) => format!("worktree {id}"),
            ContainerOwner::Checkout(path) => format!("checkout {}", path.display()),
        };
        let state = match lease.ended.map(|ending| ending.reason) {
            None => "active",
            Some(ContainerEndReason::OwnerEnded) => "owner ended",
            Some(ContainerEndReason::HoldReleased) => "hold released",
        };
        println!(
            "{} / {}: {} ({class}, {state}) owned by {owner}",
            lease.profile, lease.project, lease.id
        );
    }
    Ok(())
}

fn give_back(arguments: &[String], json: bool) -> Result<()> {
    let id = Options::parse(arguments, &[], &[])?.single("container return", "id")?;
    Barge::open()?.container_return(&id)?;
    if json {
        document("barge.container-return/1", vec![("id", json!(id))])
    } else {
        println!("returned container lease {id}");
        Ok(())
    }
}
