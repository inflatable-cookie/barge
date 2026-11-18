use std::path::PathBuf;

use barge::{
    Availability, Barge, Durability, Error, Result, VolumeRegistration, VolumeState, VolumeStatus,
};
use serde_json::json;

use crate::args::{expect_no_positionals, parse_size, Options};
use crate::human::{describe_state, format_size};
use crate::output::document;

pub(crate) fn volume(arguments: &[String], json: bool) -> Result<()> {
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
            } else if barge.has_unimported_volume_layout(&name)? {
                eprintln!("barge: volume {name} has a layout record; register with --import-layout to import it");
            }
            if json {
                document(
                    "barge.volume-registered/1",
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
            let barge = Barge::open_read_only()?;
            let entries = barge.status()?;
            if json {
                let volumes: Vec<VolumeStatus> = entries;
                document("barge.volume-list/1", vec![("volumes", json!(volumes))])?;
            } else if entries.is_empty() {
                println!("no volumes registered");
            } else {
                for entry in &entries {
                    println!(
                        "{} at {}: {}{}",
                        entry.spec.name,
                        entry.spec.mount.display(),
                        describe_state(&entry.state),
                        entry
                            .free_bytes
                            .map(|free| format!(", free {}", format_size(free)))
                            .unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        "status" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("volume status", "volume name")?;
            let barge = Barge::open_read_only()?;
            let volume = barge.volume(&name)?;
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
