//! `barge worktree`: lease a checkout from a pool, read a lease back, return it,
//! and see pool status. The mechanics are in `barge::worktree`.

use std::time::Duration;

use barge::worktree::{LeaseRequest, WorktreeLease, WorktreePool};
use barge::{Barge, Error, Mode, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::output::document;

pub(crate) fn worktree(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("worktree needs lease, return or status"));
    };
    let rest = &arguments[1..];
    match subcommand {
        "lease" if rest.first().map(String::as_str) == Some("get") => lease_get(&rest[1..], json),
        "lease" => lease(rest, json),
        "return" => give_back(rest, json),
        "run" => run(rest),
        "status" => status(rest, json),
        other => Err(Error::usage(format!(
            "unknown worktree command {other:?}; use lease, lease get, return, run or status"
        ))),
    }
}

fn lease(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(
        arguments,
        &[
            "repo",
            "base",
            "base-name",
            "branch",
            "request",
            "label",
            "wait",
        ],
        &[],
    )?;
    expect_no_positionals(&options)?;
    let wait = match options.get("wait") {
        Some(text) => Duration::from_secs(parse_seconds(&text)?),
        None => Duration::ZERO,
    };
    let request = LeaseRequest {
        repo: options.required("repo")?,
        base: options.required("base")?,
        base_name: options.get("base-name"),
        branch: options.required("branch")?,
        request: options.required("request")?,
        label: options.get("label"),
    };
    let barge = Barge::open()?;
    let lease = barge.worktree_lease(request, wait)?;
    print_lease(json, &lease, "leased")
}

fn lease_get(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["request", "lease"], &[])?;
    expect_no_positionals(&options)?;
    let barge = Barge::open_read_only()?;
    let lease = match (options.get("request"), options.get("lease")) {
        (Some(request), None) => barge.worktree_lease_by_request(&request)?,
        (None, Some(lease_id)) => barge.worktree_lease_by_id(&lease_id)?,
        _ => {
            return Err(Error::usage(
                "lease get needs exactly one of --request or --lease",
            ))
        }
    };
    print_lease(json, &lease, "lease")
}

fn give_back(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["lease", "request"], &["disposable"])?;
    expect_no_positionals(&options)?;
    let lease_id = options.required("lease")?;
    let barge = Barge::open()?;
    let outcome = barge.worktree_return(
        &lease_id,
        options.get("request").as_deref(),
        options.flag("disposable"),
    )?;
    if json {
        document(
            "barge.worktree-return/1",
            vec![
                ("action", json!(outcome.action.as_str())),
                ("lease", json!(outcome.lease)),
            ],
        )
    } else {
        println!(
            "{} lease {} ({}, root {})",
            outcome.action.as_str(),
            outcome.lease.lease_id,
            outcome.lease.state.as_str(),
            outcome.lease.root_id
        );
        Ok(())
    }
}

/// Hold a leased checkout while a command runs in it. On success this replaces
/// the process with the command, so it returns only when the command could not
/// start: the error is then the answer, and nothing ran.
fn run(arguments: &[String]) -> Result<()> {
    let Some(split) = arguments.iter().position(|argument| argument == "--") else {
        return Err(Error::usage(
            "worktree run needs -- before the command to run",
        ));
    };
    let (option_arguments, command) = (&arguments[..split], &arguments[split + 1..]);
    let Some((program, command_arguments)) = command.split_first() else {
        return Err(Error::usage("worktree run needs a command after --"));
    };
    let options = Options::parse(option_arguments, &["lease"], &["exclusive"])?;
    expect_no_positionals(&options)?;
    let lease_id = options.required("lease")?;
    let mode = if options.flag("exclusive") {
        Mode::Exclusive
    } else {
        Mode::Shared
    };
    let barge = Barge::open()?;
    let held = barge.worktree_hold(&lease_id, mode)?;
    Err(held.exec(program, command_arguments))
}

fn status(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["repo"], &[])?;
    expect_no_positionals(&options)?;
    let barge = Barge::open_read_only()?;
    let pools = barge.worktree_status(options.get("repo").as_deref())?;
    if json {
        return document("barge.worktree-status/1", vec![("pools", json!(pools))]);
    }
    if pools.is_empty() {
        println!("no worktree pools are declared");
    }
    for pool in &pools {
        describe_pool(pool);
    }
    Ok(())
}

fn describe_pool(pool: &WorktreePool) {
    println!(
        "{} (pool {}): warm {}, max {}",
        pool.repo, pool.pool, pool.warm, pool.max
    );
    if let Some(reason) = &pool.unavailable {
        println!("  unavailable: {reason}");
    }
    for root in &pool.roots {
        let held = if root.held { ", held" } else { "" };
        match (&root.lease_id, &root.branch, &root.request) {
            (Some(lease_id), Some(branch), Some(request)) => println!(
                "  {} {} on {branch} for request {request} (lease {lease_id}{held})",
                root.id, root.state
            ),
            _ => println!("  {} free{held}", root.id),
        }
    }
}

/// Print one lease, as a document or as a line. `verb` names what happened.
fn print_lease(json: bool, lease: &WorktreeLease, verb: &str) -> Result<()> {
    if json {
        return document("barge.worktree-lease/1", vec![("lease", json!(lease))]);
    }
    println!(
        "{verb} {} on {} at {} (lease {}, {})",
        lease.path,
        lease.branch,
        lease.head,
        lease.lease_id,
        lease.state.as_str()
    );
    Ok(())
}

fn parse_seconds(text: &str) -> Result<u64> {
    text.parse()
        .map_err(|_| Error::usage(format!("--wait {text:?} must be whole seconds")))
}
