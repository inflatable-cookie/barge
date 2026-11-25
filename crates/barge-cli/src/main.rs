//! Barge's CLI: argument parsing, the JSON envelope, exit codes and the human
//! summary. Every mechanic lives in the `barge` library.
//!
//! Exit codes: 0 success, 1 refused (nothing changed, including a name that does
//! not exist), 2 usage error, 3 internal error. `--json` prints one document with
//! a top-level `schema` field, `barge.<kind>/<version>`.
//!
//! This file is dispatch only: each command is a module, and the document
//! envelope, the error document and the usage text are their own modules.

use std::process::ExitCode;

use barge::{Error, Result};

mod area;
mod args;
mod bind;
mod container;
mod doctor;
mod human;
mod init;
mod layout;
mod machine;
mod move_resource;
mod output;
mod prune;
mod root;
mod scratch;
mod status;
mod survey;
mod usage;
mod volume;
mod worktree;

use crate::output::report_error;
use crate::usage::{help_for, usage_text};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // Everything after the first `--` belongs to the child process of
    // `worktree run`: a `--json` or `--help` there is passed to the command
    // untouched and never read by barge. Only the arguments before it can
    // carry barge's own flags, at any position in the command line.
    let separator = arguments.iter().position(|argument| argument == "--");
    let (before, after) = match separator {
        Some(index) => (arguments[..index].to_vec(), arguments[index..].to_vec()),
        None => (arguments, Vec::new()),
    };
    let mut cleaned: Vec<String> = Vec::with_capacity(before.len());
    let mut json = false;
    let mut help = false;
    for argument in &before {
        if argument == "--json" {
            json = true;
        } else if argument == "--help" || argument == "-h" {
            help = true;
        } else {
            cleaned.push(argument.clone());
        }
    }
    if help {
        // `help_for` falls back to the top-level usage for an unknown
        // command, so `--help` always prints and never runs anything.
        if let Some(text) = help_for(&cleaned) {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
    }
    cleaned.extend(after);
    match run(&cleaned, json) {
        Ok(code) => code,
        Err(error) => {
            report_error(&error, json);
            ExitCode::from(match error.kind() {
                barge::ErrorKind::Refused | barge::ErrorKind::NotFound => 1,
                barge::ErrorKind::Usage => 2,
                _ => 3,
            })
        }
    }
}

/// Dispatch one command line. The first argument names the command; the rest
/// belong to it.
fn run(arguments: &[String], json: bool) -> Result<ExitCode> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage(usage_text()));
    };
    dispatch(command, &arguments[1..], json)
}

fn dispatch(command: &str, rest: &[String], json: bool) -> Result<ExitCode> {
    Ok(match command {
        "help" => {
            match help_for(rest) {
                Some(text) if !rest.is_empty() => {
                    print!("{text}");
                }
                _ => {
                    println!("{}", usage_text());
                }
            }
            ExitCode::SUCCESS
        }
        "--help" | "-h" => {
            println!("{}", usage_text());
            ExitCode::SUCCESS
        }
        "doctor" => doctor::doctor(rest, json)?,
        "init" => init::init(rest, json)?,
        "status" => {
            status::status(rest, json)?;
            ExitCode::SUCCESS
        }
        "layout" => {
            layout::layout(rest, json)?;
            ExitCode::SUCCESS
        }
        "path" => {
            layout::resource_path(rest, json)?;
            ExitCode::SUCCESS
        }
        "env" if json => return Err(Error::usage("env does not support --json")),
        "env" => {
            layout::resource_env(rest)?;
            ExitCode::SUCCESS
        }
        "survey" => {
            survey::survey(rest, json)?;
            ExitCode::SUCCESS
        }
        "volume" => {
            volume::volume(rest, json)?;
            ExitCode::SUCCESS
        }
        "area" => {
            area::area(rest, json)?;
            ExitCode::SUCCESS
        }
        "root" => {
            root::root(rest, json)?;
            ExitCode::SUCCESS
        }
        "bind" => {
            bind::bind(rest, json)?;
            ExitCode::SUCCESS
        }
        "move" => {
            move_resource::move_resource(rest, json)?;
            ExitCode::SUCCESS
        }
        "prune" => {
            prune::prune(rest, json)?;
            ExitCode::SUCCESS
        }
        "scratch" => {
            scratch::scratch(rest, json)?;
            ExitCode::SUCCESS
        }
        "machine" => {
            machine::machine(rest, json)?;
            ExitCode::SUCCESS
        }
        "container" => {
            container::container(rest, json)?;
            ExitCode::SUCCESS
        }
        "worktree" => {
            worktree::worktree(rest, json)?;
            ExitCode::SUCCESS
        }
        other => {
            return Err(Error::usage(format!(
                "unknown command {other:?}\n\n{}",
                usage_text()
            )))
        }
    })
}
