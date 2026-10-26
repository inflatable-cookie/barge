//! `barge`: argument parsing, the JSON envelope, exit codes and the human
//! summary. Every rule lives in the library, so the CLI and the library cannot
//! diverge (stage 1 design, "Crate and CLI skeleton").
//!
//! Every command prints exactly one JSON document on stdout with `--json` and
//! exits `0`; a failure prints one `barge.error/1` document and exits `2` for a
//! refusal that changed nothing or `3` for unknown or unsettled state. `describe`
//! and `inspect` mutate nothing. The `area` verbs are slice S3's: `declare`,
//! `show` and `list` manage declarations, `root create` and `root list` create
//! and observe owned roots, and `hold` and `release` record cooperative custody.
//! Each mutating verb refuses before any effect when the registry is invalid, the
//! volume is absent or mismatched, or the volume's durability is undeclared.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use barge::area::{
    self, AbsentPolicy, AreaDeclaration, DeclareReport, HoldReport, HoldRequest, HoldScope,
    ListReport, PrunePolicy, ReleaseReport, ReserveParticipation, RootCreateReport, RootKind,
    RootListReport, RootSpec, ShowReport,
};
use barge::contract::{
    self, AREA_DECLARE_SCHEMA, AREA_HOLD_SCHEMA, AREA_LIST_SCHEMA, AREA_RELEASE_SCHEMA,
    AREA_ROOT_CREATE_SCHEMA, AREA_ROOT_LIST_SCHEMA, AREA_SHOW_SCHEMA, DESCRIBE_SCHEMA,
    INSPECT_SCHEMA, ROOT_ROWS,
};
use barge::describe::{
    self, DescribeData, DescribeRequest, InspectData, InspectRequest, StateDirSource,
};
use barge::error::{Error, ErrorKind};
use barge::os::{self, VolumeUuid};
use barge::state::{self, StatePaths};

const USAGE: &str = "usage: barge <describe|inspect|area> [--json]
       barge area declare --namespace <ns> --area <name> --folder <folder> [--override <uuid>=<folder>]... --absent <fail:drive|wait|fallback:path> --prune <none|oldest-idle:seconds> --reserve <counts|excluded>
       barge area show --namespace <ns> --area <name>
       barge area list
       barge area root create --namespace <ns> --area <name> --volume <uuid> --kind <area_root|cache_root|scratch_run>
       barge area root list --namespace <ns> --area <name> --volume <uuid>
       barge area hold --root <root-id> --holder <id> --pid <pid> --scope <read|write> --purpose <text>
       barge area release --root <root-id> --holder <id>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|argument| argument == "--json");
    let command = command_name(&args);

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
            Err(error) => fail(&report.command(), &error, json),
        },
        Err(error) => fail(&command, &error, json),
    }
}

/// The command as an error document names it.
fn command_name(args: &[String]) -> String {
    let mut words = args
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .take(2);
    match words.next().map(String::as_str) {
        Some("area") => match words.next() {
            Some(verb) => format!("area.{verb}"),
            None => "area".to_owned(),
        },
        Some(other) => other.to_owned(),
        None => "barge".to_owned(),
    }
}

/// One command's finished document.
enum Report {
    /// `barge.describe/1`.
    Describe(Box<DescribeData>),
    /// `barge.inspect/1`.
    Inspect(Box<InspectData>),
    /// One `area` command document, already serialised by its library type.
    Area(Box<AreaReport>),
}

struct AreaReport {
    schema: &'static str,
    command: &'static str,
    data: serde_json::Value,
    human: String,
}

impl Report {
    fn command(&self) -> String {
        match self {
            Self::Describe(_) => describe::DESCRIBE_COMMAND.to_owned(),
            Self::Inspect(_) => describe::INSPECT_COMMAND.to_owned(),
            Self::Area(report) => report.command.to_owned(),
        }
    }

    fn area<T: serde::Serialize>(
        schema: &'static str,
        command: &'static str,
        data: &T,
        human: String,
    ) -> Result<Self, Error> {
        let data = serde_json::to_value(data).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("could not encode the {schema} document: {error}"),
            )
        })?;
        Ok(Self::Area(Box::new(AreaReport {
            schema,
            command,
            data,
            human,
        })))
    }
}

/// Parse the arguments and run the one command they name.
///
/// The first word that is neither `--json` nor `--` is the command. `describe`
/// and `inspect` take no further argument; everything after `area` belongs to
/// that verb, which parses its own options.
fn dispatch(args: &[String]) -> Result<Report, Error> {
    let mut command = None;
    for (index, argument) in args.iter().enumerate() {
        if argument == "--json" || argument == "--" {
            continue;
        }
        command = Some((index, argument.as_str()));
        break;
    }
    let Some((index, command)) = command else {
        return Err(usage("a command is required"));
    };
    let rest = &args[index + 1..];
    match command {
        describe::DESCRIBE_COMMAND => {
            no_extra_arguments(rest)?;
            describe().map(Box::new).map(Report::Describe)
        }
        describe::INSPECT_COMMAND => {
            no_extra_arguments(rest)?;
            inspect().map(Box::new).map(Report::Inspect)
        }
        "area" => area(rest),
        other => Err(usage(format!("unknown command: {other}"))),
    }
}

/// Refuse any argument after a command that takes none.
fn no_extra_arguments(rest: &[String]) -> Result<(), Error> {
    match rest
        .iter()
        .find(|argument| argument.as_str() != "--json" && argument.as_str() != "--")
    {
        Some(argument) => Err(usage(format!("unexpected argument: {argument}"))),
        None => Ok(()),
    }
}

/// The values one command line gives for one option name.
type Options = BTreeMap<String, Vec<String>>;

/// One command line's positional words and its `--key value` options.
struct Parsed {
    words: Vec<String>,
    options: Options,
}

/// Parse one command line's positional words and `--key value` options.
fn parse(args: &[String]) -> Result<Parsed, Error> {
    let mut words = Vec::new();
    let mut options: Options = BTreeMap::new();
    let mut rest_positional = false;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--json" {
            index += 1;
            continue;
        }
        if argument == "--" {
            rest_positional = true;
            index += 1;
            continue;
        }
        if !rest_positional && argument.starts_with("--") {
            let key = argument.trim_start_matches("--").to_owned();
            if key.is_empty() {
                return Err(usage("an empty option name"));
            }
            let Some(value) = args.get(index + 1) else {
                return Err(usage(format!("--{key} needs a value")));
            };
            if value.starts_with("--") {
                return Err(usage(format!("--{key} needs a value")));
            }
            options.entry(key).or_default().push(value.clone());
            index += 2;
            continue;
        }
        words.push(argument.clone());
        index += 1;
    }
    Ok(Parsed { words, options })
}

fn required(options: &mut BTreeMap<String, Vec<String>>, key: &str) -> Result<String, Error> {
    let mut values = options.remove(key).unwrap_or_default();
    if values.len() > 1 {
        return Err(usage(format!("--{key} was given more than once")));
    }
    values
        .pop()
        .ok_or_else(|| usage(format!("--{key} is required")))
}

fn repeated(options: &mut Options, key: &str) -> Vec<String> {
    options.remove(key).unwrap_or_default()
}

fn refuse_unknown(options: Options) -> Result<(), Error> {
    match options.keys().next() {
        Some(key) => Err(usage(format!("unknown option for this command: --{key}"))),
        None => Ok(()),
    }
}

/// Run one `area` verb.
fn area(args: &[String]) -> Result<Report, Error> {
    let Parsed { words, options } = parse(args)?;
    match words.first().map(String::as_str) {
        Some("declare") if words.len() == 1 => area_declare(options),
        Some("show") if words.len() == 1 => area_show(options),
        Some("list") if words.len() == 1 => area_list(options),
        Some("root") => area_root(&words[1..], options),
        Some("hold") if words.len() == 1 => area_hold(options),
        Some("release") if words.len() == 1 => area_release(options),
        _ => Err(usage(format!("unknown area verb: {}", words.join(" ")))),
    }
}

fn area_declare(mut options: Options) -> Result<Report, Error> {
    let namespace = required(&mut options, "namespace")?;
    let name = required(&mut options, "area")?;
    let folder = required(&mut options, "folder")?;
    let absent = parse_absent(&required(&mut options, "absent")?)?;
    let prune = parse_prune(&required(&mut options, "prune")?)?;
    let reserve = parse_reserve(&required(&mut options, "reserve")?)?;
    let mut overrides = BTreeMap::new();
    for raw in repeated(&mut options, "override") {
        let (volume, folder) = raw.split_once('=').ok_or_else(|| {
            usage(format!(
                "--override takes <volume-uuid>=<folder>, not `{raw}`"
            ))
        })?;
        VolumeUuid::parse(volume)?;
        overrides.insert(volume.to_owned(), folder.to_owned());
    }
    refuse_unknown(options)?;
    let declaration = AreaDeclaration {
        namespace,
        area: name,
        folder,
        overrides,
        absent,
        prune,
        reserve,
    };
    let state = state_paths()?;
    let document = area::declare(&state, &declaration, state::now_ms())?;
    let path = area::declaration_path(&state, &document.namespace, &document.area)?;
    let human = format!(
        "declared {}/{} at {} under {}, generation {}\n",
        document.namespace,
        document.area,
        document.folder,
        path.display(),
        document.generation
    );
    Report::area(
        AREA_DECLARE_SCHEMA,
        "area.declare",
        &DeclareReport {
            path,
            declaration: document,
        },
        human,
    )
}

fn area_show(mut options: Options) -> Result<Report, Error> {
    let namespace = required(&mut options, "namespace")?;
    let name = required(&mut options, "area")?;
    refuse_unknown(options)?;
    let state = state_paths()?;
    let path = area::declaration_path(&state, &namespace, &name)?;
    let document = area::show(&state, &namespace, &name)?.ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            format!("area {namespace}/{name} is not declared"),
        )
        .with_detail("namespace", namespace.clone().into())
        .with_detail("area", name.clone().into())
    })?;
    let human = format!(
        "area {}/{}: folder {}, generation {}\n",
        document.namespace, document.area, document.folder, document.generation
    );
    Report::area(
        AREA_SHOW_SCHEMA,
        "area.show",
        &ShowReport {
            path,
            declaration: document,
        },
        human,
    )
}

fn area_list(options: Options) -> Result<Report, Error> {
    refuse_unknown(options)?;
    let state = state_paths()?;
    let declarations = area::read_declarations(&state)?;
    let areas = declarations.rows();
    let human = format!(
        "{} of {} declared area{}\n",
        areas.count,
        areas.total,
        if areas.count == 1 { "" } else { "s" }
    );
    Report::area(AREA_LIST_SCHEMA, "area.list", &ListReport { areas }, human)
}

fn area_root(
    words: &[String],
    mut options: BTreeMap<String, Vec<String>>,
) -> Result<Report, Error> {
    match words.first().map(String::as_str) {
        Some("create") if words.len() == 1 => {
            let namespace = required(&mut options, "namespace")?;
            let name = required(&mut options, "area")?;
            let volume = VolumeUuid::parse(&required(&mut options, "volume")?)?;
            let kind = parse_kind(&required(&mut options, "kind")?)?;
            refuse_unknown(options)?;
            let state = state_paths()?;
            let stored = require_declaration(&state, &namespace, &name)?;
            let declaration = stored.declaration();
            let platform = os::platform();
            let (volume, root) = area::create_root_on_volume(
                &state,
                &platform,
                &platform,
                &platform,
                &platform,
                &volume,
                &declaration,
                Some(&stored),
                RootSpec { kind },
            )?;
            let human = format!("created root {} at {}\n", root.root_id, root.path.display());
            Report::area(
                AREA_ROOT_CREATE_SCHEMA,
                "area.root.create",
                &RootCreateReport {
                    volume_uuid: root.volume_uuid.as_str().to_owned(),
                    folder: volume.resolved.folder.clone(),
                    area_path: volume.resolved.path.clone(),
                    effective: volume.resolved.declaration.clone(),
                    root,
                },
                human,
            )
        }
        Some("list") if words.len() == 1 => {
            let namespace = required(&mut options, "namespace")?;
            let name = required(&mut options, "area")?;
            let volume = VolumeUuid::parse(&required(&mut options, "volume")?)?;
            refuse_unknown(options)?;
            let state = state_paths()?;
            let stored = require_declaration(&state, &namespace, &name)?;
            let declaration = stored.declaration();
            let platform = os::platform();
            let (volume, listing) = area::list_roots_on_volume(
                &state,
                &platform,
                &platform,
                &platform,
                &platform,
                &volume,
                &declaration,
                Some(&stored),
                ROOT_ROWS,
            )?;
            let human = format!(
                "{} of {} root{} under {}\n",
                listing.roots.len(),
                listing.examined,
                if listing.roots.len() == 1 { "" } else { "s" },
                volume.resolved.path.display()
            );
            Report::area(
                AREA_ROOT_LIST_SCHEMA,
                "area.root.list",
                &RootListReport {
                    volume_uuid: volume.record.volume_uuid.as_str().to_owned(),
                    folder: volume.resolved.folder.clone(),
                    area_path: volume.resolved.path.clone(),
                    effective: volume.resolved.declaration.clone(),
                    roots: listing.rows(),
                    examined: listing.examined,
                    truncated: listing.truncated,
                    skipped_non_directories: listing.skipped_non_directories,
                },
                human,
            )
        }
        _ => Err(usage(format!(
            "unknown area root verb: {}",
            words.join(" ")
        ))),
    }
}

fn area_hold(mut options: Options) -> Result<Report, Error> {
    let root_id = required(&mut options, "root")?;
    let holder = required(&mut options, "holder")?;
    let pid = required(&mut options, "pid")?
        .parse::<u32>()
        .map_err(|_| usage("--pid is a process id"))?;
    let scope = match required(&mut options, "scope")?.as_str() {
        "read" => HoldScope::Read,
        "write" => HoldScope::Write,
        other => return Err(usage(format!("unknown hold scope: {other}"))),
    };
    let purpose = required(&mut options, "purpose")?;
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let held = area::hold(
        &state,
        &platform,
        &HoldRequest {
            root_id,
            holder,
            pid,
            scope,
            purpose,
        },
    )?;
    let row = area::HoldRow {
        holder: held.record.holder.clone(),
        state: area::HoldStateName::Live,
        scope: held.record.scope,
        purpose: held.record.purpose.clone(),
        pid: held.record.pid,
        held_ms: held.record.held_ms,
        process_start: held.record.process_start,
        boot_identity: held.record.boot_identity.clone(),
        reason: None,
    };
    let human = format!(
        "held root {} as {}\n",
        held.record.root_id, held.record.holder
    );
    Report::area(
        AREA_HOLD_SCHEMA,
        "area.hold",
        &HoldReport {
            path: held.path,
            hold: row,
        },
        human,
    )
}

fn area_release(mut options: Options) -> Result<Report, Error> {
    let root_id = required(&mut options, "root")?;
    let holder = required(&mut options, "holder")?;
    refuse_unknown(options)?;
    let state = state_paths()?;
    let path = state
        .holds_dir()
        .join(&root_id)
        .join(format!("{holder}.json"));
    let released = path.is_file();
    let path = area::release(&state, &root_id, &holder)?;
    let human = if released {
        format!("released {holder} on root {root_id}\n")
    } else {
        format!("{holder} held nothing on root {root_id}\n")
    };
    Report::area(
        AREA_RELEASE_SCHEMA,
        "area.release",
        &ReleaseReport {
            path,
            released,
            root_id,
            holder,
        },
        human,
    )
}

fn require_declaration(
    state: &StatePaths,
    namespace: &str,
    name: &str,
) -> Result<area::AreaDocument, Error> {
    area::show(state, namespace, name)?.ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            format!("area {namespace}/{name} is not declared; declare it first"),
        )
        .with_detail("namespace", namespace.to_owned().into())
        .with_detail("area", name.to_owned().into())
    })
}

fn state_paths() -> Result<StatePaths, Error> {
    let platform = os::platform();
    StatePaths::resolve(&platform, state_dir_override().as_deref())
}

fn parse_absent(raw: &str) -> Result<AbsentPolicy, Error> {
    if raw == "wait" {
        return Ok(AbsentPolicy::Wait);
    }
    if let Some(drive) = raw.strip_prefix("fail:") {
        return Ok(AbsentPolicy::Fail {
            attach: drive.to_owned(),
        });
    }
    if let Some(path) = raw.strip_prefix("fallback:") {
        return Ok(AbsentPolicy::FallbackLocal {
            path: PathBuf::from(path),
        });
    }
    Err(usage(
        "--absent takes fail:<drive>, wait or fallback:<absolute path>",
    ))
}

fn parse_prune(raw: &str) -> Result<PrunePolicy, Error> {
    if raw == "none" {
        return Ok(PrunePolicy::None);
    }
    if let Some(age) = raw.strip_prefix("oldest-idle:") {
        let age_seconds = age
            .parse::<u64>()
            .map_err(|_| usage("--prune oldest-idle takes a number of seconds"))?;
        return Ok(PrunePolicy::OldestIdleAfter { age_seconds });
    }
    Err(usage("--prune takes none or oldest-idle:<seconds>"))
}

fn parse_reserve(raw: &str) -> Result<ReserveParticipation, Error> {
    match raw {
        "counts" => Ok(ReserveParticipation::Counts),
        "excluded" => Ok(ReserveParticipation::Excluded),
        other => Err(usage(format!("unknown reserve participation: {other}"))),
    }
}

fn parse_kind(raw: &str) -> Result<RootKind, Error> {
    match raw {
        "area_root" => Ok(RootKind::AreaRoot),
        "cache_root" => Ok(RootKind::CacheRoot),
        "scratch_run" => Ok(RootKind::ScratchRun),
        other => Err(usage(format!("unknown root kind: {other}"))),
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
        dirs: &platform,
        facts: &platform,
        mounts: &platform,
        observer: &platform,
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
        Report::Area(data) => {
            if json {
                let bytes = contract::success_bytes(data.schema, data.command, &data.data)?;
                write_stdout(&bytes)
            } else {
                write_stdout(data.human.as_bytes())
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
        "barge {} ({})\nstate directory: {} ({state}, from {})\nhome: {}\nvolumes: {} of {}; areas: {} of {}; placements: {} of {}; roots: {} of {}; observations: {} of {}\ncustody: {} (kernel proof: {})\n",
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
        data.roots.count,
        data.roots.total,
        data.observations.count,
        data.observations.total,
        data.custody.capability,
        data.custody.kernel_proof,
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
