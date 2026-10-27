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
//! The `cache` verbs are slice S6's: `adopt` records one package-cache placement
//! (and writes a tool setting only when the explicit adopt asks for it), `undo`
//! reverses exactly the recorded form, and `list` reports every placement.
//! Each mutating verb refuses before any effect when the registry is invalid, the
//! volume is absent or mismatched, or the volume's durability is undeclared.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use barge::area::{
    self, AbsentPolicy, AreaDeclaration, DeclareReport, HoldReport, HoldRequest, HoldScope,
    ListReport, PrunePolicy, ReleaseReport, ReserveParticipation, RootCreateReport, RootKind,
    RootListReport, RootSpec, ShowReport,
};
use barge::cache::{self, AdoptRequest, CacheKind, FormRequest, UndoRequest};
use barge::contract::{
    self, AREA_DECLARE_SCHEMA, AREA_HOLD_SCHEMA, AREA_LIST_SCHEMA, AREA_RELEASE_SCHEMA,
    AREA_ROOT_CREATE_SCHEMA, AREA_ROOT_LIST_SCHEMA, AREA_SHOW_SCHEMA, CACHE_ADOPT_SCHEMA,
    CACHE_LIST_SCHEMA, CACHE_UNDO_SCHEMA, DESCRIBE_SCHEMA, DOCUMENT_SIZE_CAP_BYTES, INSPECT_SCHEMA,
    MAINTAIN_SCHEMA, ROOT_ROWS, SCRATCH_ALLOC_SCHEMA, SCRATCH_CLAIM_SCHEMA,
    SCRATCH_CONCLUDE_SCHEMA, SCRATCH_DELEGATE_SCHEMA, SCRATCH_EXPIRE_SCHEMA, SCRATCH_GRANTS_SCHEMA,
    SCRATCH_REVOKE_SCHEMA,
};
use barge::describe::{
    self, DescribeData, DescribeRequest, InspectData, InspectRequest, StateDirSource,
};
use barge::error::{Error, ErrorKind};
use barge::os::{self, VolumeUuid};
use barge::prune::{self, MaintainBounds, MaintainReport, MaintainRequest};
use barge::scratch::{
    self, AllocRequest, ConcludeRequest, ExpireReport, SweepBounds, SweepRequest,
};
use barge::state::{self, StatePaths};

const USAGE: &str = "usage: barge <describe|inspect|maintain|area|cache|diagnostic|scratch> [--json]
       barge maintain [--plan-out <file>]
       barge maintain --plan <file> --plan-sha256 <hex>
       barge diagnostic descriptor-census --root <existing-directory>
       barge area declare --namespace <ns> --area <name> --folder <folder> [--override <uuid>=<folder>]... --absent <fail:drive|wait|fallback:path> --prune <none|oldest-idle:seconds> --reserve <counts|excluded>
       barge area show --namespace <ns> --area <name>
       barge area list
       barge area root create --namespace <ns> --area <name> --volume <uuid> --kind <area_root|cache_root|scratch_run>
       barge area root list --namespace <ns> --area <name> --volume <uuid>
       barge area hold --root <root-id> --holder <id> --pid <pid> --scope <read|write> --purpose <text>
       barge area release --root <root-id> --holder <id>
       barge cache adopt --kind <npm-cache|bun-cache|cargo-registry|cargo-git> --volume <uuid> [--form <setting|held-root|link>] [--write-setting] [--adopt-existing-root] [--plan]
       barge cache undo (--placement <id> | --kind <kind>) [--plan]
       barge cache list
       barge scratch alloc --volume <uuid> --project <name> --consumer <id> [--pid <pid>]
       barge scratch conclude --volume <uuid> --root <root-id> --holder <id> [--project <name>] --outcome <closed|unclosed|failed|retained> --closure <positive|none> --kept-reason <none|evidence|unclosed|exception|nested-unclosed> --nested-scope <exclusive|hosting>
       barge scratch expire --volume <uuid> [--plan] [--plan-sha256 <hex>]
       barge scratch grants --volume <uuid>
       barge scratch delegate --volume <uuid> --family <oxide_scratch|effigy_scratch> --by <name> [--revoke]
       barge scratch claim --volume <uuid> --scope <project:name|top-level-adhoc> --unit <child|entry> --idle-seconds <n> [--exclude-family <family>]... --declared-by <name> [--revoke]";

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
            Ok(()) => ExitCode::from(report.exit()),
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
    let first = words.next().map(String::as_str);
    match first {
        Some(command @ ("area" | "scratch")) => match words.next() {
            Some(verb) => format!("{command}.{verb}"),
            None => command.to_owned(),
        },
        Some("cache") => match words.next() {
            Some(verb) => format!("cache.{verb}"),
            None => "cache".to_owned(),
        },
        Some("diagnostic") => "diagnostic.descriptor-census".to_owned(),
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
    /// One `area` or `cache` command document, already serialised by its library
    /// type.
    Verb(Box<VerbReport>),
    /// A command whose finished document carries its own exit code: a `scratch`
    /// verb, or `maintain`.
    Outcome(Box<OutcomeReport>),
}

struct OutcomeReport {
    schema: &'static str,
    command: &'static str,
    data: serde_json::Value,
    human: String,
    exit: u8,
}

struct VerbReport {
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
            Self::Verb(report) => report.command.to_owned(),
            Self::Outcome(report) => report.command.to_owned(),
        }
    }

    /// The exit code a finished document forces: `3` when a scratch expiry left
    /// an intent that needs replay, `0` otherwise.
    fn exit(&self) -> u8 {
        match self {
            Self::Outcome(report) => report.exit,
            _ => 0,
        }
    }

    fn outcome<T: serde::Serialize>(
        schema: &'static str,
        command: &'static str,
        data: &T,
        human: String,
        exit: u8,
    ) -> Result<Self, Error> {
        let data = serde_json::to_value(data).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("could not encode the {schema} document: {error}"),
            )
        })?;
        Ok(Self::Outcome(Box::new(OutcomeReport {
            schema,
            command,
            data,
            human,
            exit,
        })))
    }

    fn verb<T: serde::Serialize>(
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
        Ok(Self::Verb(Box::new(VerbReport {
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
        "maintain" => maintain(rest),
        "area" => area(rest),
        "cache" => cache(rest),
        "diagnostic" => descriptor_census(rest),
        "scratch" => scratch(rest),
        other => Err(usage(format!("unknown command: {other}"))),
    }
}

/// Read-only native apparatus through the same binary and product observer.
fn descriptor_census(args: &[String]) -> Result<Report, Error> {
    let Parsed { words, mut options } = parse(args)?;
    if words != ["descriptor-census"] {
        return Err(usage("expected diagnostic descriptor-census"));
    }
    let root = PathBuf::from(required(&mut options, "root")?);
    refuse_unknown(options)?;
    let platform = os::platform();
    let report = barge::diagnostic::descriptor_census(&platform, &platform, &root)?;
    let human = format!(
        "descriptor census: {:?}; {:?}\n",
        report.status, report.problem
    );
    Report::verb(
        contract::DESCRIPTOR_CENSUS_SCHEMA,
        "diagnostic.descriptor-census",
        &report,
        human,
    )
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

/// The options that take no value.
const FLAGS: [&str; 4] = ["plan", "write-setting", "adopt-existing-root", "revoke"];

/// One command line's positional words and its `--key value` options.
struct Parsed {
    words: Vec<String>,
    options: Options,
}

/// Parse one command line's positional words and `--key value` options.
fn parse(args: &[String]) -> Result<Parsed, Error> {
    parse_taking(args, &[])
}

/// [`parse`] with option names that take a value for this command even though
/// they are a flag elsewhere: `maintain`'s `--plan` names the preview bytes,
/// while `scratch expire` and `cache adopt` take it as a bare flag.
fn parse_taking(args: &[String], value_options: &[&str]) -> Result<Parsed, Error> {
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
            // A flag carries no value; every other option takes exactly one.
            if FLAGS.contains(&key.as_str()) && !value_options.contains(&key.as_str()) {
                options.entry(key).or_default().push("true".to_owned());
                index += 1;
                continue;
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

/// Read one optional single-valued option, refusing a repeated one.
fn one_option(options: &mut Options, key: &str) -> Result<Option<String>, Error> {
    match options.remove(key) {
        None => Ok(None),
        Some(mut values) if values.len() == 1 => Ok(values.pop()),
        Some(_) => Err(usage(format!("--{key} was given more than once"))),
    }
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
    Report::verb(
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
    Report::verb(
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
    Report::verb(AREA_LIST_SCHEMA, "area.list", &ListReport { areas }, human)
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
            Report::verb(
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
            Report::verb(
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
    Report::verb(
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
    Report::verb(
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

/// Run one `maintain` run: a preview, or a real run the preview authorized.
///
/// The preview is effect-free and prints one bounded canonical
/// `barge.maintain.plan/1` with its SHA-256. A real run requires the preview's
/// exact bytes (`--plan <file>`) and their digest (`--plan-sha256`), so the
/// units it acts on are exactly the ones the preview named; each unit is
/// re-verified immediately before its effect, and a unit that changed in between
/// is kept and reported as that unit only.
fn maintain(args: &[String]) -> Result<Report, Error> {
    let Parsed { words, mut options } = parse_taking(args, &["plan", "plan-out"])?;
    if let Some(word) = words.first() {
        return Err(usage(format!("unexpected argument: {word}")));
    }
    let plan_file = one_option(&mut options, "plan")?.map(PathBuf::from);
    let plan_out = one_option(&mut options, "plan-out")?.map(PathBuf::from);
    let plan_digest = one_option(&mut options, "plan-sha256")?;
    let preview = plan_digest.is_none();
    if let Some(path) = &plan_file {
        if preview {
            return Err(usage(format!(
                "--plan {} is the preview's exact bytes for a real run, so it needs --plan-sha256; nothing was changed",
                path.display()
            )));
        }
    }
    if plan_digest.is_some() && plan_file.is_none() {
        return Err(usage(
            "a real run needs the preview's exact plan bytes (--plan <file>) as well as --plan-sha256; nothing was changed",
        ));
    }
    if plan_out.is_some() && !preview {
        return Err(usage(
            "--plan-out writes a preview's plan and has no meaning on a real run; nothing was changed",
        ));
    }
    if plan_file.is_some() && plan_out.is_some() {
        return Err(usage(
            "--plan and --plan-out name different files; give one",
        ));
    }
    refuse_unknown(options)?;
    let plan_bytes = match &plan_file {
        Some(path) => Some(read_plan_file(path)?),
        None => None,
    };
    let state = state_paths()?;
    let platform = os::platform();
    let report = prune::maintain(
        &state,
        &platform,
        &platform,
        &platform,
        &platform,
        &MaintainRequest {
            preview,
            plan_digest: plan_digest.as_deref(),
            plan_bytes: plan_bytes.as_deref(),
            sweep_bounds: SweepBounds::default(),
            delete_bounds: barge::delete::DeleteBounds::default(),
            maintain_bounds: MaintainBounds::design(),
            now_ms: state::now_ms(),
        },
    )?;
    if preview {
        if let Some(path) = &plan_out {
            let bytes = prune::canonical_plan_bytes(&report.plan)?;
            write_plan_file(path, &bytes)?;
        }
    }
    let human = maintain_summary(&report);
    let exit = if report.needs_human() { 3 } else { 0 };
    Report::outcome(MAINTAIN_SCHEMA, "maintain", &report, human, exit)
}

/// Read the caller's plan file, bounded before it is parsed.
fn read_plan_file(path: &Path) -> Result<Vec<u8>, Error> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not read the plan file {}: {error}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(Error::new(
            ErrorKind::Usage,
            format!("the plan file {} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > DOCUMENT_SIZE_CAP_BYTES as u64 {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the plan file {} is {} bytes, over the {DOCUMENT_SIZE_CAP_BYTES}-byte cap",
                path.display(),
                metadata.len()
            ),
        ));
    }
    std::fs::read(path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not read the plan file {}: {error}", path.display()),
        )
    })
}

/// Write a preview's exact plan bytes where the caller asked for them.
fn write_plan_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::write(path, bytes).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not write the plan file {}: {error}", path.display()),
        )
    })
}

/// The human summary of `barge.maintain/1`.
fn maintain_summary(report: &MaintainReport) -> String {
    let mut lines = Vec::new();
    // A preview has no volume outcomes, so its loud rows come from the plan.
    for volume in &report.plan.volumes.rows {
        if report.preview && volume.loud {
            if let Some(detail) = &volume.detail {
                lines.push(format!("warning: {detail}"));
            }
        }
    }
    for volume in &report.volumes.rows {
        if volume.loud {
            if let Some(detail) = &volume.detail {
                lines.push(format!("warning: {detail}"));
            }
        }
        if let Some(stage) = &volume.scratch {
            lines.push(format!(
                "{}: scratch {} deleted, {} pending, {} skipped",
                volume.volume_uuid, stage.deleted.count, stage.pending.count, stage.skipped.count
            ));
        }
        for area in &volume.areas {
            lines.push(format!(
                "{}: {}/{} {} deleted, {} pending, {} skipped",
                volume.volume_uuid,
                area.namespace,
                area.area,
                area.deleted.count,
                area.pending.count,
                area.skipped.count
            ));
        }
    }
    if report.timed_out {
        lines.push(format!(
            "the run's wall clock stopped it before {} stage(s)",
            report.unreached.count
        ));
    }
    if let Some(stopped) = &report.stopped {
        lines.push(format!("stopped: {stopped}"));
    }
    let head = if report.preview {
        format!(
            "previewed maintain plan {}: {} candidate(s) on {} volume(s); nothing was changed",
            report.plan_digest, report.candidates, report.plan.volumes.count
        )
    } else {
        format!(
            "maintained {} volume(s) under plan {}: {} candidate(s), {} pending",
            report.volumes.count, report.plan_digest, report.candidates, report.pending.count
        )
    };
    lines.push(head);
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Run one `scratch` verb.
fn scratch(args: &[String]) -> Result<Report, Error> {
    let Parsed { words, options } = parse(args)?;
    match words.first().map(String::as_str) {
        Some("alloc") if words.len() == 1 => scratch_alloc(options),
        Some("conclude") if words.len() == 1 => scratch_conclude(options),
        Some("expire") if words.len() == 1 => scratch_expire(options),
        Some("grants") if words.len() == 1 => scratch_grants(options),
        Some("delegate") if words.len() == 1 => scratch_delegate(options),
        Some("claim") if words.len() == 1 => scratch_claim(options),
        _ => Err(usage(format!("unknown scratch verb: {}", words.join(" ")))),
    }
}

fn volume_arg(raw: &str) -> Result<VolumeUuid, Error> {
    VolumeUuid::parse(raw).map_err(|error| usage(format!("--volume: {}", error.message())))
}

fn scratch_alloc(mut options: Options) -> Result<Report, Error> {
    let volume = volume_arg(&required(&mut options, "volume")?)?;
    let project = required(&mut options, "project")?;
    let consumer = required(&mut options, "consumer")?;
    let pid = match options.remove("pid") {
        Some(mut values) => {
            if values.len() > 1 {
                return Err(usage("--pid was given more than once"));
            }
            values
                .pop()
                .unwrap_or_default()
                .parse::<u32>()
                .map_err(|_| usage("--pid is a process id"))?
        }
        None => std::process::id(),
    };
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let report = scratch::alloc(
        &state,
        &platform,
        &platform,
        &platform,
        &platform,
        &AllocRequest {
            volume_uuid: volume,
            project,
            consumer,
            pid,
        },
    )?;
    let human = format!(
        "allocated scratch root {} at {} for {}\n",
        report.root.root_id,
        report.root.path.display(),
        report.consumer
    );
    Report::outcome(SCRATCH_ALLOC_SCHEMA, "scratch.alloc", &report, human, 0)
}

fn scratch_conclude(mut options: Options) -> Result<Report, Error> {
    let volume = volume_arg(&required(&mut options, "volume")?)?;
    let root = required(&mut options, "root")?;
    let holder = required(&mut options, "holder")?;
    let project = options.remove("project").map(|mut values| {
        if values.len() > 1 {
            values.pop();
            values.pop()
        } else {
            values.pop()
        }
    });
    let project = match project {
        Some(Some(value)) => Some(value),
        Some(None) => return Err(usage("--project was given more than once")),
        None => None,
    };
    let outcome = parse_outcome(&required(&mut options, "outcome")?)?;
    let closure = match required(&mut options, "closure")?.as_str() {
        "positive" => barge::area::ConclusionClosure::Positive,
        "none" => barge::area::ConclusionClosure::None,
        other => return Err(usage(format!("unknown closure: {other}"))),
    };
    let kept_reason = parse_kept_reason(&required(&mut options, "kept-reason")?)?;
    let nested_scope = match required(&mut options, "nested-scope")?.as_str() {
        "exclusive" => barge::area::NestedScope::Exclusive,
        "hosting" => barge::area::NestedScope::Hosting,
        other => return Err(usage(format!("unknown nested scope: {other}"))),
    };
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let report = scratch::conclude(
        &state,
        &platform,
        &platform,
        &platform,
        &platform,
        &ConcludeRequest {
            volume_uuid: &volume,
            root_id: &root,
            holder: &holder,
            project: project.as_deref(),
            outcome,
            closure,
            kept_reason,
            nested_scope,
        },
    )?;
    let human = format!(
        "concluded scratch root {} at {}\n",
        report.root_id,
        report.path.display()
    );
    Report::outcome(
        SCRATCH_CONCLUDE_SCHEMA,
        "scratch.conclude",
        &report,
        human,
        0,
    )
}

fn scratch_expire(mut options: Options) -> Result<Report, Error> {
    let volume = volume_arg(&required(&mut options, "volume")?)?;
    let preview = options
        .remove("plan")
        .is_some_and(|values| !values.is_empty());
    let plan_digest = options
        .remove("plan-sha256")
        .map(|mut values| values.pop().unwrap_or_default());
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let report = scratch::sweep(
        &state,
        &platform,
        &platform,
        &platform,
        &platform,
        &SweepRequest {
            volume_uuid: &volume,
            preview,
            plan_digest: plan_digest.as_deref(),
            bounds: SweepBounds::default(),
            delete_bounds: barge::delete::DeleteBounds::default(),
            now_ms: state::now_ms(),
        },
    )?;
    let human = expire_summary(&report);
    let exit = if report.has_pending() { 3 } else { 0 };
    Report::outcome(
        SCRATCH_EXPIRE_SCHEMA,
        "scratch.expire",
        &report,
        human,
        exit,
    )
}

fn scratch_grants(mut options: Options) -> Result<Report, Error> {
    let volume = volume_arg(&required(&mut options, "volume")?)?;
    refuse_unknown(options)?;
    let state = state_paths()?;
    let report = scratch::grants(&state, &volume)?;
    let human = format!(
        "{} delegation(s), {} claim(s) on {}\n",
        report.grants.delegations.len(),
        report.grants.claims.len(),
        report.volume_uuid
    );
    Report::outcome(SCRATCH_GRANTS_SCHEMA, "scratch.grants", &report, human, 0)
}

fn scratch_delegate(mut options: Options) -> Result<Report, Error> {
    let volume = volume_arg(&required(&mut options, "volume")?)?;
    let family = match required(&mut options, "family")?.as_str() {
        "oxide_scratch" => barge::registry::MarkerFamily::OxideScratch,
        "effigy_scratch" => barge::registry::MarkerFamily::EffigyScratch,
        other => return Err(usage(format!("unknown marker family: {other}"))),
    };
    let by = required(&mut options, "by")?;
    let revoke = options
        .remove("revoke")
        .is_some_and(|values| !values.is_empty());
    refuse_unknown(options)?;
    let state = state_paths()?;
    let report =
        scratch::declare_delegation(&state, &volume, family, &by, state::now_ms(), revoke)?;
    let human = format!(
        "{} family {:?} on {}\n",
        if revoke { "withdrew" } else { "recorded" },
        family,
        report.volume_uuid
    );
    let schema = if revoke {
        SCRATCH_REVOKE_SCHEMA
    } else {
        SCRATCH_DELEGATE_SCHEMA
    };
    Report::outcome(
        schema,
        if revoke {
            "scratch.revoke"
        } else {
            "scratch.delegate"
        },
        &report,
        human,
        0,
    )
}

fn scratch_claim(mut options: Options) -> Result<Report, Error> {
    use barge::registry::{ClaimScope, ClaimUnit, MarkerFamily, ScratchClaim};
    let volume = volume_arg(&required(&mut options, "volume")?)?;
    let raw_scope = required(&mut options, "scope")?;
    let scope = match raw_scope.as_str() {
        "top-level-adhoc" | "top_level_adhoc" => ClaimScope::TopLevelAdhoc,
        other => match other.strip_prefix("project:") {
            Some(name) if !name.is_empty() => ClaimScope::Project {
                name: name.to_owned(),
            },
            _ => return Err(usage("--scope takes project:<name> or top-level-adhoc")),
        },
    };
    let unit = match required(&mut options, "unit")?.as_str() {
        "child" => ClaimUnit::Child,
        "entry" => ClaimUnit::Entry,
        other => return Err(usage(format!("unknown claim unit: {other}"))),
    };
    let mut families_excluded = Vec::new();
    for raw in repeated(&mut options, "exclude-family") {
        families_excluded.push(match raw.as_str() {
            "oxide_scratch" => MarkerFamily::OxideScratch,
            "effigy_scratch" => MarkerFamily::EffigyScratch,
            other => return Err(usage(format!("unknown marker family: {other}"))),
        });
    }
    let revoke = options
        .remove("revoke")
        .is_some_and(|values| !values.is_empty());
    let declared_by = if revoke {
        options
            .remove("declared-by")
            .and_then(|mut values| values.pop())
            .unwrap_or_else(|| "revoke".to_owned())
    } else {
        required(&mut options, "declared-by")?
    };
    let idle_seconds = if revoke {
        options
            .remove("idle-seconds")
            .and_then(|mut values| values.pop())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1)
    } else {
        required(&mut options, "idle-seconds")?
            .parse::<u64>()
            .map_err(|_| usage("--idle-seconds is a number of seconds"))?
    };
    refuse_unknown(options)?;
    let claim = ScratchClaim {
        scope,
        unit,
        unmarked_idle_seconds: idle_seconds,
        families_excluded,
        declared_by,
        declared_ms: state::now_ms(),
    };
    let state = state_paths()?;
    let report = scratch::declare_claim(&state, &volume, &claim, revoke)?;
    let human = format!(
        "{} claim on {}\n",
        if revoke { "withdrew" } else { "recorded" },
        report.volume_uuid
    );
    let schema = if revoke {
        SCRATCH_REVOKE_SCHEMA
    } else {
        SCRATCH_CLAIM_SCHEMA
    };
    Report::outcome(
        schema,
        if revoke {
            "scratch.revoke"
        } else {
            "scratch.claim"
        },
        &report,
        human,
        0,
    )
}

fn expire_summary(report: &ExpireReport) -> String {
    let plan = &report.plan;
    let replay = report.replay.as_ref().map_or_else(String::new, |replay| {
        format!(
            "replay: {} settled, {} resumed, {} pending; ",
            replay.settled,
            replay.resumed,
            replay.pending.len()
        )
    });
    format!(
        "{}scratch plan {}: {} candidate(s), {} kept, {} examined{}, deleted {}, pending {}; {replay}{}\n",
        if report.preview { "previewed " } else { "" },
        report.plan_digest,
        plan.candidates.count,
        plan.kept.count,
        plan.examined,
        if plan.truncated { " (bounded)" } else { "" },
        report.deleted.count,
        report.pending.count,
        if report.volume_present { "volume attached" } else { "volume absent" },
    )
}

fn parse_outcome(raw: &str) -> Result<barge::area::ConclusionOutcome, Error> {
    match raw {
        "closed" => Ok(barge::area::ConclusionOutcome::Closed),
        "unclosed" => Ok(barge::area::ConclusionOutcome::Unclosed),
        "failed" => Ok(barge::area::ConclusionOutcome::Failed),
        "retained" => Ok(barge::area::ConclusionOutcome::Retained),
        other => Err(usage(format!("unknown outcome: {other}"))),
    }
}

fn parse_kept_reason(raw: &str) -> Result<barge::area::KeptReason, Error> {
    match raw {
        "none" => Ok(barge::area::KeptReason::None),
        "evidence" => Ok(barge::area::KeptReason::Evidence),
        "unclosed" => Ok(barge::area::KeptReason::Unclosed),
        "exception" => Ok(barge::area::KeptReason::Exception),
        "nested-unclosed" => Ok(barge::area::KeptReason::NestedUnclosed),
        other => Err(usage(format!("unknown kept reason: {other}"))),
    }
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

/// Run one `cache` verb.
fn cache(args: &[String]) -> Result<Report, Error> {
    let Parsed { words, options } = parse(args)?;
    match words.first().map(String::as_str) {
        Some("adopt") if words.len() == 1 => cache_adopt(options),
        Some("undo") if words.len() == 1 => cache_undo(options),
        Some("list") if words.len() == 1 => cache_list(options),
        _ => Err(usage(format!("unknown cache verb: {}", words.join(" ")))),
    }
}

fn cache_adopt(mut options: Options) -> Result<Report, Error> {
    let kind = CacheKind::parse(&required(&mut options, "kind")?).map_err(with_usage)?;
    let volume = VolumeUuid::parse(&required(&mut options, "volume")?).map_err(with_usage)?;
    let form = match options.remove("form") {
        Some(values) if values.len() > 1 => return Err(usage("--form was given more than once")),
        Some(mut values) => Some(FormRequest::parse(&values.remove(0)).map_err(with_usage)?),
        None => None,
    };
    let write_setting = flag(&mut options, "write-setting")?;
    let adopt_existing_root = flag(&mut options, "adopt-existing-root")?;
    let plan = flag(&mut options, "plan")?;
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let request = AdoptRequest {
        kind,
        volume_uuid: &volume,
        form,
        write_setting,
        adopt_existing_root,
        plan,
    };
    let report = cache::adopt(
        &state, &platform, &platform, &platform, &platform, &platform, &request,
    )?;
    let human = if report.plan {
        format!(
            "plan for {} on volume {volume}: {} change(s), nothing was written\n{}\n",
            kind.as_str(),
            report.changes.len(),
            report.changes.join("\n")
        )
    } else {
        let id = report
            .placement
            .as_ref()
            .map_or_else(String::new, |placement| placement.placement_id.clone());
        format!(
            "adopted {} as placement {id} on volume {volume}\n{}\n",
            kind.as_str(),
            report.changes.join("\n")
        )
    };
    Report::verb(CACHE_ADOPT_SCHEMA, "cache.adopt", &report, human)
}

fn cache_undo(mut options: Options) -> Result<Report, Error> {
    let placement_id = options
        .remove("placement")
        .map(|mut values| values.remove(0));
    let kind = match options.remove("kind") {
        Some(values) if values.len() > 1 => return Err(usage("--kind was given more than once")),
        Some(mut values) => Some(CacheKind::parse(&values.remove(0)).map_err(with_usage)?),
        None => None,
    };
    if placement_id.is_some() == kind.is_some() {
        return Err(usage(
            "cache undo takes exactly one of --placement or --kind",
        ));
    }
    let plan = flag(&mut options, "plan")?;
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let report = cache::undo(
        &state,
        &platform,
        &platform,
        &platform,
        &platform,
        &UndoRequest {
            placement_id: placement_id.as_deref(),
            kind,
            plan,
        },
    )?;
    let human = if report.plan {
        format!(
            "plan to undo placement {}: {} change(s), nothing was written\n{}\n",
            report.placement_id,
            report.changes.len(),
            report.changes.join("\n")
        )
    } else {
        format!(
            "undid placement {}\n{}\n",
            report.placement_id,
            report.changes.join("\n")
        )
    };
    Report::verb(CACHE_UNDO_SCHEMA, "cache.undo", &report, human)
}

fn cache_list(options: Options) -> Result<Report, Error> {
    refuse_unknown(options)?;
    let state = state_paths()?;
    let platform = os::platform();
    let report = cache::list(&state, &platform, &platform, &platform, &platform)?;
    let human = format!(
        "{} of {} placement{}\n",
        report.placements.count,
        report.placements.total,
        if report.placements.count == 1 {
            ""
        } else {
            "s"
        }
    );
    Report::verb(CACHE_LIST_SCHEMA, "cache.list", &report, human)
}

/// Give a usage refusal the command's usage text, so every usage error names it.
fn with_usage(error: Error) -> Error {
    if error.kind() == ErrorKind::Usage && !error.detail().contains_key("usage") {
        return error.with_detail("usage", USAGE.into());
    }
    error
}

/// Read one boolean flag: present is true, absent is false, and a repeated flag
/// is a usage refusal rather than a guess.
fn flag(options: &mut Options, key: &str) -> Result<bool, Error> {
    match options.remove(key) {
        None => Ok(false),
        Some(values) if values.len() == 1 => Ok(values[0] == "true"),
        Some(_) => Err(usage(format!("--{key} was given more than once"))),
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
        Report::Verb(data) => {
            if json {
                let bytes = contract::success_bytes(data.schema, data.command, &data.data)?;
                write_stdout(&bytes)
            } else {
                write_stdout(data.human.as_bytes())
            }
        }
        Report::Outcome(data) => {
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
