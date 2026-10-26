//! `barge`: argument parsing, the JSON envelope, exit codes and the human
//! summary. Every rule lives in the library, so the CLI and the library cannot
//! diverge (stage 1 design, "Crate and CLI skeleton").
//!
//! `describe --json` and `inspect --json` each print exactly one JSON document
//! on stdout and exit `0`. A failure prints one `barge.error/1` document on
//! stdout when `--json` was asked for, or a human line on stderr otherwise, and
//! exits `2` for a refusal that changed nothing or `3` for unknown or unsettled
//! state. Neither command mutates anything: slice S2 has no command that writes.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use barge::contract::{self, DESCRIBE_SCHEMA, INSPECT_SCHEMA};
use barge::describe::{
    self, DescribeData, DescribeRequest, InspectData, InspectRequest, StateDirSource,
};
use barge::error::{Error, ErrorKind};
use barge::os;

const USAGE: &str = "usage: barge <describe|inspect> [--json]";

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
        Ok(report) => match emit(&report, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&command, &error, json),
        },
        Err(error) => fail(&command, &error, json),
    }
}

/// One command's finished document. The boxes keep the enum one word wide, so a
/// large document never moves through the stack twice.
enum Report {
    /// `barge.describe/1`.
    Describe(Box<DescribeData>),
    /// `barge.inspect/1`.
    Inspect(Box<InspectData>),
}

/// Parse the arguments and run the one command they name. Every other shape is a
/// usage refusal, which changes nothing.
fn dispatch(args: &[String]) -> Result<Report, Error> {
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
        Some(describe::DESCRIBE_COMMAND) => describe().map(Box::new).map(Report::Describe),
        Some(describe::INSPECT_COMMAND) => inspect().map(Box::new).map(Report::Inspect),
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
    let state_dir_override = state_dir_override();
    describe::describe(&DescribeRequest {
        account: &platform,
        environment_home: environment_home.as_deref(),
        state_dir_override: state_dir_override.as_deref(),
        tool_version: env!("CARGO_PKG_VERSION"),
    })
}

/// Read the inspect document's data through the platform's OS layer.
fn inspect() -> Result<InspectData, Error> {
    let platform = os::platform();
    let environment_home = std::env::var_os("HOME").map(PathBuf::from);
    let state_dir_override = state_dir_override();
    describe::inspect(&InspectRequest {
        account: &platform,
        environment_home: environment_home.as_deref(),
        state_dir_override: state_dir_override.as_deref(),
        tool_version: env!("CARGO_PKG_VERSION"),
    })
}

fn state_dir_override() -> Option<PathBuf> {
    std::env::var_os(barge::state::STATE_DIR_ENV).map(PathBuf::from)
}

/// Print the success document, JSON or human, on stdout.
fn emit(report: &Report, json: bool) -> Result<(), Error> {
    match report {
        Report::Describe(data) => {
            if json {
                let bytes =
                    contract::success_bytes(DESCRIBE_SCHEMA, describe::DESCRIBE_COMMAND, data)?;
                write_stdout(&bytes)
            } else {
                write_stdout(describe_summary(data).as_bytes())
            }
        }
        Report::Inspect(data) => {
            if json {
                let bytes =
                    contract::success_bytes(INSPECT_SCHEMA, describe::INSPECT_COMMAND, data)?;
                write_stdout(&bytes)
            } else {
                write_stdout(inspect_summary(data).as_bytes())
            }
        }
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

/// The human summary of `barge.describe/1`.
fn describe_summary(data: &DescribeData) -> String {
    let state = if data.state.initialized {
        "initialized"
    } else {
        "not initialized"
    };
    format!(
        "barge {} ({})\nstate directory: {} ({state}, from {})\nhome: {}\nvolumes: {} of {}; areas: {} of {}; placements: {} of {}; observations: {} of {}\n",
        data.tool.version,
        data.formats.describe,
        data.state.dir.display(),
        source_name(data.state.source),
        home_line(&data.state.home),
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

/// The human summary of `barge.inspect/1`.
fn inspect_summary(data: &InspectData) -> String {
    let state = if data.state.initialized {
        "initialized"
    } else {
        "not initialized"
    };
    let registry = match &data.registry.state {
        describe::RegistryReportState::Absent => "absent".to_owned(),
        describe::RegistryReportState::Valid => format!(
            "{} volume{}",
            data.registry.volumes.count,
            if data.registry.volumes.count == 1 {
                ""
            } else {
                "s"
            }
        ),
        describe::RegistryReportState::Invalid => "invalid".to_owned(),
    };
    let oxide = &data.oxide;
    format!(
        "barge {} ({INSPECT_SCHEMA})\nstate directory: {} ({state}, from {})\nhome: {}\nregistry: {} ({registry})\noxide registration: {} ({})\noxide pools: {} ({})\n",
        data.tool.version,
        data.state.dir.display(),
        source_name(data.state.source),
        home_line(&data.state.home),
        data.registry.path.display(),
        oxide.registration.path.display(),
        observation_name(oxide.registration.state),
        oxide.pools.path.display(),
        observation_name(oxide.pools.state),
    )
}

fn source_name(source: StateDirSource) -> &'static str {
    match source {
        StateDirSource::Account => "account",
        StateDirSource::EnvironmentOverride => barge::state::STATE_DIR_ENV,
    }
}

fn home_line(home: &describe::HomeReport) -> String {
    match &home.environment {
        Some(environment) if home.mismatch => format!(
            "{} (HOME is {} and differs; the account home is used)",
            home.account.display(),
            environment.display()
        ),
        _ => home.account.display().to_string(),
    }
}

fn observation_name(state: describe::ObservationState) -> &'static str {
    match state {
        describe::ObservationState::Reported => "reported",
        describe::ObservationState::Absent => "absent",
        describe::ObservationState::UnknownState => "unknown state",
    }
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage, message).with_detail("usage", USAGE.into())
}
