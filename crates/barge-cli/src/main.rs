//! `barge`: argument parsing, the JSON envelope, exit codes and the human
//! summary. Every rule lives in the library, so the CLI and the library cannot
//! diverge (stage 1 design, "Crate and CLI skeleton").
//!
//! `describe --json` prints exactly one JSON document on stdout and exits `0`.
//! A failure prints one `barge.error/1` document on stdout when `--json` was
//! asked for, or a human line on stderr otherwise, and exits `2` for a refusal
//! that changed nothing or `3` for unknown or unsettled state.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use barge::contract::{self, DESCRIBE_SCHEMA};
use barge::describe::{self, DescribeData, DescribeRequest, StateDirSource};
use barge::error::{Error, ErrorKind};
use barge::os;

const USAGE: &str = "usage: barge describe [--json]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|argument| argument == "--json");
    let command = args
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map_or("barge", String::as_str)
        .to_owned();

    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        return match write_stdout(format!("{USAGE}\n").as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&command, &error, json),
        };
    }

    match dispatch(&args) {
        Ok(data) => match emit(&data, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&command, &error, json),
        },
        Err(error) => fail(&command, &error, json),
    }
}

/// Parse the arguments and run the one command S1 has. Every other shape is a
/// usage refusal, which changes nothing.
fn dispatch(args: &[String]) -> Result<DescribeData, Error> {
    let mut command: Option<&str> = None;
    for argument in args {
        match argument.as_str() {
            "--json" => {}
            other if other.starts_with('-') => {
                return Err(usage(format!("unknown option: {other}")));
            }
            other if command.is_none() => command = Some(other),
            other => return Err(usage(format!("unexpected argument: {other}"))),
        }
    }

    match command {
        Some(describe::DESCRIBE_COMMAND) => describe(),
        Some(other) => Err(usage(format!("unknown command: {other}"))),
        None => Err(usage("a command is required")),
    }
}

/// Read the describe document's data through the platform's OS layer.
fn describe() -> Result<DescribeData, Error> {
    let platform = os::platform();
    let environment_home = std::env::var_os("HOME").map(PathBuf::from);
    // `BARGE_STATE_DIR` exists for fixtures and proofs; a normal run leaves it
    // unset, so the account's authority cannot move by accident.
    let state_dir_override = std::env::var_os("BARGE_STATE_DIR").map(PathBuf::from);
    describe::describe(&DescribeRequest {
        account: &platform,
        environment_home: environment_home.as_deref(),
        state_dir_override: state_dir_override.as_deref(),
        tool_version: env!("CARGO_PKG_VERSION"),
    })
}

/// Print the success document, JSON or human, on stdout.
fn emit(data: &DescribeData, json: bool) -> Result<(), Error> {
    if json {
        let bytes = contract::success_bytes(DESCRIBE_SCHEMA, describe::DESCRIBE_COMMAND, data)?;
        write_stdout(&bytes)
    } else {
        write_stdout(human_summary(data).as_bytes())
    }
}

/// Print the failure: one error document on stdout with `--json`, otherwise a
/// human line on stderr. The exit code comes from the failure's kind.
fn fail(command: &str, error: &Error, json: bool) -> ExitCode {
    if json {
        let bytes = contract::error_bytes(command, error);
        if write_stdout(&bytes).is_err() {
            eprintln!("barge: {}: {error}", error.kind());
        }
    } else {
        eprintln!("barge: {}: {error}", error.kind());
    }
    ExitCode::from(error.kind().exit_code())
}

fn write_stdout(bytes: &[u8]) -> Result<(), Error> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .map_err(|error| Error::new(ErrorKind::Io, format!("could not write to stdout: {error}")))
}

/// The human summary: the same facts as the document, for a terminal.
fn human_summary(data: &DescribeData) -> String {
    let state = if data.state.initialized {
        "initialized"
    } else {
        "not initialized"
    };
    let source = match data.state.source {
        StateDirSource::Account => "account",
        StateDirSource::EnvironmentOverride => "BARGE_STATE_DIR",
    };
    let home = match &data.state.home.environment {
        Some(environment) if data.state.home.mismatch => format!(
            "{} (HOME is {} and differs; the account home is used)",
            data.state.home.account.display(),
            environment.display()
        ),
        _ => data.state.home.account.display().to_string(),
    };
    format!(
        "barge {} ({})\nstate directory: {} ({state}, from {source})\nhome: {home}\nvolumes: {} of {}; areas: {} of {}; placements: {} of {}; observations: {} of {}\n",
        data.tool.version,
        data.formats.describe,
        data.state.dir.display(),
        data.volumes.count,
        data.volumes.total,
        data.areas.count,
        data.areas.total,
        data.placements.count,
        data.placements.total,
        data.observations.count,
        data.observations.total,
    )
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage, message).with_detail("usage", USAGE.into())
}
