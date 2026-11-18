//! Barge's CLI: argument parsing, the JSON envelope, exit codes and the human
//! summary. Every mechanic lives in the `barge` library.
//!
//! Exit codes: 0 success, 1 refused (nothing changed, including a name that does
//! not exist), 2 usage error, 3 internal error. `--json` prints one document with a top-level `schema`
//! field, `barge.<kind>/<version>`.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use barge::{
    Absent, AreaSpec, Availability, Barge, Class, Durability, Error, ErrorKind, Placement, Result,
    RootInfo, VolumeRegistration, VolumeState, VolumeStatus,
};
use serde_json::{json, Value};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let json = arguments.iter().any(|argument| argument == "--json");
    let arguments: Vec<String> = arguments
        .into_iter()
        .filter(|argument| argument != "--json")
        .collect();
    match run(&arguments, json) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error, json);
            ExitCode::from(match error.kind() {
                ErrorKind::Refused | ErrorKind::NotFound => 1,
                ErrorKind::Usage => 2,
                _ => 3,
            })
        }
    }
}

fn run(arguments: &[String], json: bool) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage(usage_text()));
    };
    let rest = &arguments[1..];
    match command {
        "help" | "--help" | "-h" => {
            println!("{}", usage_text());
            Ok(())
        }
        "status" => status(rest, json),
        "layout" => layout(rest, json),
        "path" => resource_path(rest, json),
        "env" if json => Err(Error::usage("env does not support --json")),
        "env" => resource_env(rest),
        "survey" => survey(rest, json),
        "volume" => volume(rest, json),
        "area" => area(rest, json),
        "root" => root(rest, json),
        "prune" => prune(rest, json),
        "scratch" => scratch(rest, json),
        other => Err(Error::usage(format!(
            "unknown command {other:?}\n\n{}",
            usage_text()
        ))),
    }
}

fn usage_text() -> String {
    "\
barge — storage volumes, areas, roots, holds, pins, pruning and scratch

usage: barge [--json] <command>

  layout show|set                          inspect or change placements
  path <class> [--project P]              resolve a resource path
  env [--shell zsh|bash|fish]             shell assignments
  survey <volume> [--no-bytes]            inspect top-level items
  status                                  every volume, area and free space
  volume register --name N --mount PATH   register a volume (reads its UUID)
    [--uuid U] [--root-dir DIR] [--availability permanent|ephemeral]
    [--durability expendable|kept] [--reserve SIZE] [--floor SIZE] [--import-layout]
  volume list                             registered volumes and their state
  volume status <name>                    one volume's identity and space
  area declare --namespace NS --name A --volume V
    [--folder DIR] [--absent fail|fallback:PATH] [--soft-cap SIZE]
    [--counts-toward-reserve true|false]
  area list [--volume V]                  declared areas and their location
  root list <namespace>/<area> [--bytes]   roots, holds, pins and stragglers
  root pin <namespace>/<area> <id> [--reason TEXT]
  root unpin <namespace>/<area> <id>
  root evict <namespace>/<area> <id>      rename into trash, then delete
  prune --volume V [--dry-run]            evict idle roots to meet policy
  scratch new --volume V --project P --label L [--ttl SECONDS]
  scratch sweep --volume V [--project P]  delete expired idle scratch roots

SIZES: integer bytes, or KiB, MiB, GiB, TiB.
"
    .to_string()
}

fn layout(arguments: &[String], json: bool) -> Result<()> {
    let (command, rest) = arguments
        .split_first()
        .ok_or_else(|| Error::usage("layout needs show or set"))?;
    match command.as_str() {
        "show" => {
            let options = Options::parse(rest, &[], &[])?;
            expect_no_positionals(&options)?;
            let barge = Barge::open_read_only()?;
            let layout = barge.layout()?;
            let mut disagreements = Vec::new();
            for volume in barge.volumes()? {
                if matches!(volume.state()?, VolumeState::Present)
                    && barge.layout_disagrees(volume.name())?
                {
                    disagreements.push(volume.name().to_string());
                }
            }
            if json {
                document(
                    "barge.layout-show/1",
                    vec![
                        ("layout", json!(layout)),
                        ("disagreements", json!(disagreements)),
                    ],
                )?;
            } else {
                for (class, placement) in &layout.classes {
                    println!(
                        "{}: {} / {}",
                        class.name(),
                        placement.volume,
                        placement.path
                    );
                }
                for name in disagreements {
                    eprintln!("barge: layout record on {name} disagrees with account");
                }
            }
            Ok(())
        }
        "set" => {
            let options = Options::parse(rest, &["volume", "path", "absent"], &[])?;
            let class = options.single("layout set", "class")?.parse::<Class>()?;
            let barge = Barge::open()?;
            let absent = match options.get("absent") {
                Some(value) => parse_absent(&value)?,
                None => barge
                    .layout()?
                    .classes
                    .get(&class)
                    .map(|p| p.absent.clone())
                    .unwrap_or(Absent::Fail),
            };
            let placement = Placement {
                volume: options.required("volume")?,
                path: options.required("path")?,
                absent,
            };
            let result = barge.set_layout(class, placement)?;
            if json {
                document("barge.layout/1", vec![("layout", json!(result))])?;
            } else {
                println!("updated {} layout", class.name());
            }
            Ok(())
        }
        _ => Err(Error::usage("layout needs show or set")),
    }
}

fn resource_path(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["project"], &[])?;
    let class = options.single("path", "class")?.parse::<Class>()?;
    let resolved =
        Barge::open_read_only()?.resource_path(class, options.get("project").as_deref())?;
    if resolved.fallback {
        eprintln!("barge: using fallback for {}", class.name());
    }
    if json {
        document("barge.path/1", vec![("path", json!(resolved))])?;
    } else {
        println!("{}", resolved.path.display());
    }
    Ok(())
}

fn resource_env(arguments: &[String]) -> Result<()> {
    let options = Options::parse(arguments, &["shell"], &[])?;
    expect_no_positionals(&options)?;
    let shell = options.get("shell").unwrap_or_else(|| "zsh".to_string());
    if !["zsh", "bash", "fish"].contains(&shell.as_str()) {
        return Err(Error::usage("shell must be zsh, bash or fish"));
    }
    let barge = Barge::open_read_only()?;
    match barge.resource_env(&shell) {
        Ok((assignment, fallback)) => {
            if fallback {
                eprintln!("barge: using fallback for scratch");
            }
            println!("{assignment}");
        }
        Err(error) if matches!(error.kind(), ErrorKind::Refused | ErrorKind::NotFound) => {
            eprintln!("barge: {}", error.message())
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn survey(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &[], &["no-bytes"])?;
    let name = options.single("survey", "volume")?;
    let items = Barge::open_read_only()?.survey(&name, !options.flag("no-bytes"))?;
    if json {
        document(
            "barge.survey/1",
            vec![("volume", json!(name)), ("items", json!(items))],
        )?;
    } else {
        for item in items {
            println!(
                "{}  {}{}",
                item.path.display(),
                item.classification,
                item.bytes
                    .map(|b| format!("  {}", format_size(b)))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn status(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &[], &[])?;
    expect_no_positionals(&options)?;
    let barge = Barge::open()?;
    let status = barge.status()?;
    if json {
        document("barge.status/1", vec![("volumes", json!(status))])?;
    } else {
        for volume in &status {
            println!("{}", volume_line(volume));
            for area in &volume.areas {
                println!("  {}", area_line(area));
            }
        }
    }
    Ok(())
}

fn document(schema: &str, fields: Vec<(&str, Value)>) -> Result<()> {
    let mut data = serde_json::Map::new();
    for (name, value) in fields {
        data.insert(name.to_string(), value);
    }
    let document = json!({ "schema": schema, "data": Value::Object(data) });
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{document}")
        .map_err(|source| Error::new(ErrorKind::Io, format!("cannot write to stdout: {source}")))?;
    Ok(())
}

fn volume(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("volume needs register, list or status"));
    };
    let rest = &arguments[1..];
    match subcommand {
        "register" => {
            let options = Options::parse(
                rest,
                &[
                    "name",
                    "mount",
                    "uuid",
                    "root-dir",
                    "availability",
                    "durability",
                    "reserve",
                    "floor",
                ],
                &["import-layout"],
            )?;
            expect_no_positionals(&options)?;
            let name = options.required("name")?;
            let mount = PathBuf::from(options.required("mount")?);
            let mut registration = VolumeRegistration::new(name.clone(), mount);
            if let Some(uuid) = options.get("uuid") {
                registration = registration.uuid(Some(uuid));
            }
            if let Some(root_dir) = options.get("root-dir") {
                registration = registration.root_dir(root_dir);
            }
            if let Some(availability) = options.get("availability") {
                registration = registration.availability(match availability.as_str() {
                    "permanent" => Availability::Permanent,
                    "ephemeral" => Availability::Ephemeral,
                    other => {
                        return Err(Error::usage(format!(
                            "--availability {other:?} must be permanent or ephemeral"
                        )))
                    }
                });
            }
            if let Some(durability) = options.get("durability") {
                registration = registration.durability(match durability.as_str() {
                    "expendable" => Durability::Expendable,
                    "kept" => Durability::Kept,
                    other => {
                        return Err(Error::usage(format!(
                            "--durability {other:?} must be expendable or kept"
                        )))
                    }
                });
            }
            if let Some(reserve) = options.get("reserve") {
                registration = registration.reserve_bytes(parse_size(&reserve)?);
            }
            if let Some(floor) = options.get("floor") {
                registration = registration.floor_bytes(parse_size(&floor)?);
            }
            let barge = Barge::open()?;
            let volume = barge.register_volume(registration)?;
            let state = volume.state()?;
            if options.flag("import-layout") {
                barge.import_volume_layout(&name)?;
            } else if barge.volume_layout(&name)?.is_some()
                && barge.layout()?.classes.values().all(|p| p.volume != name)
            {
                eprintln!("barge: volume {name} has a layout record; register with --import-layout to import it");
            }
            if json {
                document(
                    "barge.volume/1",
                    vec![("volume", json!(volume.spec())), ("state", json!(state))],
                )?;
            } else {
                println!(
                    "registered volume {} at {} ({})",
                    volume.name(),
                    volume.spec().mount.display(),
                    describe_state(&state)
                );
            }
            Ok(())
        }
        "list" => {
            let options = Options::parse(rest, &[], &[])?;
            expect_no_positionals(&options)?;
            let barge = Barge::open()?;
            let mut entries: Vec<(barge::VolumeSpec, VolumeState, Option<u64>)> = Vec::new();
            for volume in barge.volumes()? {
                let state = volume.state()?;
                let free_bytes = match state {
                    VolumeState::Present => volume.free_bytes().ok(),
                    _ => None,
                };
                entries.push((volume.spec().clone(), state, free_bytes));
            }
            if json {
                let volumes: Vec<Value> = entries
                    .iter()
                    .map(|(spec, state, free_bytes)| {
                        json!({ "spec": spec, "state": state, "free_bytes": free_bytes })
                    })
                    .collect();
                document("barge.volumes/1", vec![("volumes", Value::Array(volumes))])?;
            } else if entries.is_empty() {
                println!("no volumes registered");
            } else {
                for (spec, state, free_bytes) in &entries {
                    println!(
                        "{} at {}: {}{}",
                        spec.name,
                        spec.mount.display(),
                        describe_state(state),
                        free_bytes
                            .map(|free| format!(", free {}", format_size(free)))
                            .unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        "status" => {
            let options = Options::parse(rest, &[], &[])?;
            let names = options.positionals;
            let name = names
                .first()
                .ok_or_else(|| Error::usage("volume status needs a volume name"))?;
            let barge = Barge::open()?;
            let volume = barge.volume(name)?;
            let state = volume.state()?;
            let free_bytes = match state {
                VolumeState::Present => Some(volume.free_bytes()?),
                _ => None,
            };
            if json {
                document(
                    "barge.volume/1",
                    vec![
                        ("volume", json!(volume.spec())),
                        ("state", json!(state)),
                        ("free_bytes", json!(free_bytes)),
                    ],
                )?;
            } else {
                println!(
                    "{} at {}: {}",
                    volume.name(),
                    volume.spec().mount.display(),
                    describe_state(&state)
                );
                if let Some(free) = free_bytes {
                    println!(
                        "  free {}  reserve {}  floor {}",
                        format_size(free),
                        format_size(volume.spec().reserve_bytes),
                        format_size(volume.spec().floor_bytes)
                    );
                }
            }
            Ok(())
        }
        other => Err(Error::usage(format!(
            "unknown volume command {other:?}; use register, list or status"
        ))),
    }
}

fn area(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("area needs declare or list"));
    };
    let rest = &arguments[1..];
    match subcommand {
        "declare" => {
            let options = Options::parse(
                rest,
                &[
                    "namespace",
                    "name",
                    "volume",
                    "folder",
                    "absent",
                    "soft-cap",
                    "counts-toward-reserve",
                ],
                &[],
            )?;
            expect_no_positionals(&options)?;
            let namespace = options.required("namespace")?;
            let name = options.required("name")?;
            let volume = options.required("volume")?;
            let mut spec = AreaSpec::new(namespace, name, volume);
            if let Some(folder) = options.get("folder") {
                spec = spec.folder(folder);
            }
            if let Some(absent) = options.get("absent") {
                spec = spec.absent(parse_absent(&absent)?);
            }
            if let Some(soft_cap) = options.get("soft-cap") {
                spec = spec.soft_cap_bytes(Some(parse_size(&soft_cap)?));
            }
            if let Some(counts) = options.get("counts-toward-reserve") {
                spec = spec.counts_toward_reserve(parse_bool("counts-toward-reserve", &counts)?);
            }
            let barge = Barge::open()?;
            let area = barge.area(spec)?;
            let status = area.status()?;
            if json {
                document("barge.area/1", vec![("area", json!(status))])?;
            } else {
                println!("declared area {}", area_line(&status));
            }
            Ok(())
        }
        "list" => {
            let options = Options::parse(rest, &["volume"], &[])?;
            expect_no_positionals(&options)?;
            let barge = Barge::open()?;
            let mut areas = Vec::new();
            for area in barge.areas()? {
                if let Some(volume) = options.get("volume") {
                    if area.record().volume != volume {
                        continue;
                    }
                }
                areas.push(area.status()?);
            }
            if json {
                document("barge.areas/1", vec![("areas", json!(areas))])?;
            } else if areas.is_empty() {
                println!("no areas declared");
            } else {
                for area in &areas {
                    println!("{}", area_line(area));
                }
            }
            Ok(())
        }
        other => Err(Error::usage(format!(
            "unknown area command {other:?}; use declare or list"
        ))),
    }
}

fn root(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("root needs list, pin, unpin or evict"));
    };
    let rest = &arguments[1..];
    let barge = Barge::open()?;
    match subcommand {
        "list" => {
            let options = Options::parse(rest, &[], &["bytes"])?;
            let key = options.single("root list", "area")?;
            let area = area_for(&barge, &key)?;
            let listing = if options.flag("bytes") {
                area.list()?
            } else {
                area.list_without_bytes()?
            };
            if json {
                document(
                    "barge.roots/1",
                    vec![
                        ("area", json!(area.key().label())),
                        ("bytes_included", json!(options.flag("bytes"))),
                        ("roots", json!(listing.roots)),
                        ("stragglers", json!(listing.stragglers)),
                    ],
                )?;
            } else {
                if listing.roots.is_empty() {
                    println!("no roots in {}", area.key().label());
                }
                for root in &listing.roots {
                    if options.flag("bytes") {
                        println!(
                            "{} {} {}",
                            root.id,
                            format_size(root.bytes),
                            describe_root(root)
                        );
                    } else {
                        println!("{} {}", root.id, describe_root(root));
                    }
                }
                for straggler in &listing.stragglers {
                    println!("kept {}: {}", straggler.path.display(), straggler.reason);
                }
            }
            Ok(())
        }
        "pin" | "unpin" | "evict" => {
            let options = if subcommand == "pin" {
                Options::parse(rest, &["reason"], &[])?
            } else {
                Options::parse(rest, &[], &[])?
            };
            let (key, id) = options.two_positionals(subcommand)?;
            let area = area_for(&barge, &key)?;
            match subcommand {
                "pin" => {
                    let reason = options
                        .get("reason")
                        .unwrap_or_else(|| "pinned by the operator".to_string());
                    area.pin(&id, &reason)?;
                    finish(json, "barge.root/1", &area.key().label(), &id, "pinned")?;
                }
                "unpin" => {
                    area.unpin(&id)?;
                    finish(json, "barge.root/1", &area.key().label(), &id, "unpinned")?;
                }
                _ => {
                    let eviction = area.evict(&id)?;
                    if json {
                        document(
                            "barge.root/1",
                            vec![("action", json!("evicted")), ("eviction", json!(eviction))],
                        )?;
                    } else {
                        println!(
                            "evicted {} from {} ({}), trash at {}",
                            eviction.id,
                            eviction.area,
                            format_size(eviction.bytes),
                            eviction.trash_path.display()
                        );
                    }
                }
            }
            Ok(())
        }
        other => Err(Error::usage(format!(
            "unknown root command {other:?}; use list, pin, unpin or evict"
        ))),
    }
}

fn finish(json: bool, schema: &str, area: &str, id: &str, action: &str) -> Result<()> {
    if json {
        document(
            schema,
            vec![
                ("area", json!(area)),
                ("id", json!(id)),
                ("action", json!(action)),
            ],
        )?;
    } else {
        println!("{action} {id} in {area}");
    }
    Ok(())
}

fn prune(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["volume"], &["dry-run"])?;
    expect_no_positionals(&options)?;
    let volume = options.required("volume")?;
    let dry_run = options.flag("dry-run");
    let barge = Barge::open()?;
    let report = barge.prune(&volume, dry_run)?;
    if json {
        document("barge.prune/1", vec![("prune", json!(report))])?;
    } else {
        print_prune(&report);
    }
    Ok(())
}

fn scratch(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("scratch needs new or sweep"));
    };
    let rest = &arguments[1..];
    let barge = Barge::open()?;
    match subcommand {
        "new" => {
            let options = Options::parse(rest, &["volume", "project", "label", "ttl"], &[])?;
            expect_no_positionals(&options)?;
            let volume = options.required("volume")?;
            let project = options.required("project")?;
            let label = options.required("label")?;
            let ttl = match options.get("ttl") {
                Some(text) => text.parse::<u64>().map_err(|_| {
                    Error::usage(format!("--ttl {text:?} must be a whole number of seconds"))
                })?,
                None => 3600,
            };
            let scratch = barge.scratch(&volume, &project)?;
            let root = scratch.create(&label, Duration::from_secs(ttl))?;
            if json {
                document(
                    "barge.scratch/1",
                    vec![
                        ("area", json!(scratch.area().key().label())),
                        ("root", json!(root)),
                    ],
                )?;
            } else {
                println!("{}", root.path.display());
            }
            Ok(())
        }
        "sweep" => {
            let options = Options::parse(rest, &["volume", "project"], &[])?;
            expect_no_positionals(&options)?;
            let volume = options.required("volume")?;
            let report = barge.sweep_scratch(&volume, options.get("project").as_deref())?;
            if json {
                document("barge.sweep/1", vec![("sweep", json!(report))])?;
            } else {
                print_sweep(&report);
            }
            Ok(())
        }
        other => Err(Error::usage(format!(
            "unknown scratch command {other:?}; use new or sweep"
        ))),
    }
}

fn area_for(barge: &Barge, key: &str) -> Result<barge::Area> {
    let key = barge::area::AreaKey::parse(key)?;
    let mut found = None;
    for area in barge.areas()? {
        if area.key() == key {
            found = Some(area);
            break;
        }
    }
    found.ok_or_else(|| {
        Error::not_found(format!(
            "area {} is not declared; declare it with `barge area declare`",
            key.label()
        ))
    })
}

fn print_prune(report: &barge::PruneReport) {
    let prefix = if report.dry_run {
        "would evict"
    } else {
        "evicted"
    };
    for root in &report.evicted {
        println!(
            "{prefix} {} from {} ({}, {})",
            root.id,
            root.area,
            format_size(root.bytes),
            match root.reason {
                barge::PruneReason::SoftCap => "soft cap",
                barge::PruneReason::Reserve => "reserve",
            }
        );
    }
    for root in &report.kept {
        println!(
            "kept {} from {} ({}, {})",
            root.id,
            root.area,
            format_size(root.bytes),
            match root.reason {
                barge::KeepReason::Held => "held",
                barge::KeepReason::Pinned => "pinned",
                barge::KeepReason::Refused => "refused",
            }
        );
    }
    for swept in &report.swept {
        println!(
            "swept trash {} ({})",
            swept.path.display(),
            format_size(swept.bytes)
        );
    }
    for kept in &report.trash_kept {
        println!("kept trash {}: {}", kept.path.display(), kept.reason);
    }
    println!(
        "volume {}: free {} -> {} (reserve {}, floor {})",
        report.volume,
        format_size(report.free_before_bytes),
        format_size(report.free_after_bytes),
        format_size(report.reserve_bytes),
        format_size(report.floor_bytes)
    );
}

fn print_sweep(report: &barge::SweepReport) {
    for swept in &report.deleted {
        println!(
            "deleted {} ({})",
            swept.path.display(),
            format_size(swept.bytes)
        );
    }
    for kept in &report.kept {
        println!("kept {}: {}", kept.path.display(), kept.reason);
    }
    println!(
        "deleted {} in {} entries",
        format_size(report.deleted_bytes()),
        report.deleted.len()
    );
}

fn volume_line(volume: &VolumeStatus) -> String {
    format!(
        "{} at {}: {}{}",
        volume.spec.name,
        volume.spec.mount.display(),
        describe_state(&volume.state),
        volume
            .free_bytes
            .map(|free| format!(", free {}", format_size(free)))
            .unwrap_or_default()
    )
}

fn area_line(area: &barge::AreaStatus) -> String {
    match (&area.location, &area.unavailable) {
        (Some(path), _) => format!(
            "area {}/{} at {}{} roots {}, {}{}",
            area.namespace,
            area.name,
            path.display(),
            if area.fallback { " (fallback)" } else { "" },
            area.roots,
            format_size(area.bytes),
            if area.stragglers > 0 {
                format!(", {} kept stragglers", area.stragglers)
            } else {
                String::new()
            }
        ),
        (None, Some(reason)) => format!(
            "area {}/{} unavailable: {reason}",
            area.namespace, area.name
        ),
        (None, None) => format!("area {}/{} unavailable", area.namespace, area.name),
    }
}

fn describe_state(state: &VolumeState) -> String {
    match state {
        VolumeState::Present => "present".to_string(),
        VolumeState::Absent => "absent".to_string(),
        VolumeState::Mismatch { registered, found } => {
            format!("mismatch: found {found}, registered {registered}")
        }
    }
}

fn describe_root(root: &RootInfo) -> String {
    let mut parts = Vec::new();
    parts.push(if root.held { "held" } else { "idle" }.to_string());
    if root.pinned {
        parts.push("pinned".to_string());
    }
    if let Some(expires) = root.expires_unix_ms {
        parts.push(format!("expires {expires}"));
    }
    parts.join(", ")
}

fn report_error(error: &Error, json: bool) {
    let mut stderr = std::io::stderr().lock();
    if json {
        let document = json!({
            "schema": "barge.error/1",
            "error": { "kind": error.kind().as_str(), "message": error.message() },
        });
        let _ = writeln!(stderr, "{document}");
    } else {
        let _ = writeln!(stderr, "barge: {}", error.message());
    }
}

fn parse_size(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, suffix) = trimmed.split_at(split);
    if digits.is_empty() {
        return Err(Error::usage(format!(
            "size {text:?} must start with a number"
        )));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| Error::usage(format!("size {text:?} has too many digits")))?;
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        other => {
            return Err(Error::usage(format!(
                "size {text:?} ends with {other:?}; use bytes, KiB, MiB, GiB or TiB"
            )))
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| Error::usage(format!("size {text:?} is too large")))
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn parse_bool(field: &str, text: &str) -> Result<bool> {
    match text {
        "true" | "yes" => Ok(true),
        "false" | "no" => Ok(false),
        other => Err(Error::usage(format!(
            "--{field} {other:?} must be true or false"
        ))),
    }
}

fn parse_absent(text: &str) -> Result<Absent> {
    if text == "fail" {
        return Ok(Absent::Fail);
    }
    if let Some(path) = text.strip_prefix("fallback:") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(Error::usage(format!(
                "--absent {text:?} needs an absolute fallback path"
            )));
        }
        return Ok(Absent::Fallback { path });
    }
    Err(Error::usage(
        "--absent must be fail or fallback:<absolute path>",
    ))
}

fn expect_no_positionals(options: &Options) -> Result<()> {
    if let Some(extra) = options.positionals.first() {
        return Err(Error::usage(format!("unexpected argument {extra:?}")));
    }
    Ok(())
}

/// One command's parsed options.
#[derive(Debug, Default)]
struct Options {
    values: HashMap<String, String>,
    flags: Vec<String>,
    positionals: Vec<String>,
}

impl Options {
    fn parse(arguments: &[String], value_flags: &[&str], flag_flags: &[&str]) -> Result<Self> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            index += 1;
            let Some(rest) = argument.strip_prefix("--") else {
                options.positionals.push(argument.clone());
                continue;
            };
            let (name, inline) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (rest, None),
            };
            if flag_flags.contains(&name) {
                if inline.is_some() {
                    return Err(Error::usage(format!("--{name} takes no value")));
                }
                options.flags.push(name.to_string());
                continue;
            }
            if value_flags.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        let value = arguments
                            .get(index)
                            .cloned()
                            .ok_or_else(|| Error::usage(format!("--{name} needs a value")))?;
                        index += 1;
                        value
                    }
                };
                if options.values.insert(name.to_string(), value).is_some() {
                    return Err(Error::usage(format!("--{name} was given twice")));
                }
                continue;
            }
            return Err(Error::usage(format!("unknown option --{name}")));
        }
        Ok(options)
    }

    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn required(&self, name: &str) -> Result<String> {
        self.get(name)
            .ok_or_else(|| Error::usage(format!("--{name} is required")))
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    fn single(&self, command: &str, field: &str) -> Result<String> {
        match self.positionals.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(Error::usage(format!("{command} needs <{field}>"))),
            _ => Err(Error::usage(format!("{command} takes one <{field}>"))),
        }
    }

    fn two_positionals(&self, command: &str) -> Result<(String, String)> {
        match self.positionals.as_slice() {
            [area, id] => Ok((area.clone(), id.clone())),
            _ => Err(Error::usage(format!(
                "root {command} needs <namespace>/<area> <id>"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_and_refuse() {
        assert_eq!(parse_size("10GiB").unwrap(), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1 MiB").unwrap(), 1024 * 1024);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("1PiB").is_err());
    }

    #[test]
    fn options_refuse_unknown_flags() {
        let parsed = Options::parse(
            &["--volume".to_string(), "Builds".to_string()],
            &["volume"],
            &[],
        )
        .unwrap();
        assert_eq!(parsed.get("volume").as_deref(), Some("Builds"));
        assert!(Options::parse(&["--nope".to_string()], &[], &[]).is_err());
    }

    #[test]
    fn absent_policies_parse() {
        assert_eq!(parse_absent("fail").unwrap(), Absent::Fail);
        assert_eq!(
            parse_absent("fallback:/tmp/pool").unwrap(),
            Absent::Fallback {
                path: PathBuf::from("/tmp/pool")
            }
        );
        assert!(parse_absent("fallback:relative").is_err());
    }
}
