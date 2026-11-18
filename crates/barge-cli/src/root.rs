use barge::{Barge, Error, Result};
use serde_json::json;

use crate::args::Options;
use crate::human::{describe_root, describe_size};
use crate::output::document;

pub(crate) fn root(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("root needs list, pin, unpin or evict"));
    };
    let rest = &arguments[1..];
    match subcommand {
        "list" => {
            let options = Options::parse(rest, &[], &["bytes"])?;
            let barge = Barge::open_read_only()?;
            let key = options.single("root list", "area")?;
            let area = barge.find_area(&key)?;
            let listing = if options.flag("bytes") {
                area.list()?
            } else {
                area.list_without_bytes()?
            };
            if json {
                document(
                    "barge.roots/1",
                    vec![
                        ("area_key", json!(area.key().label())),
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
                            describe_size(root.bytes, root.bytes_approximate),
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
            let barge = Barge::open()?;
            let area = barge.find_area(&key)?;
            match subcommand {
                "pin" => {
                    let reason = options
                        .get("reason")
                        .unwrap_or_else(|| "pinned by the operator".to_string());
                    area.pin(&id, &reason)?;
                    finish(
                        json,
                        "barge.root-action/1",
                        &area.key().label(),
                        &id,
                        "pinned",
                    )?;
                }
                "unpin" => {
                    area.unpin(&id)?;
                    finish(
                        json,
                        "barge.root-action/1",
                        &area.key().label(),
                        &id,
                        "unpinned",
                    )?;
                }
                _ => {
                    let eviction = area.evict(&id)?;
                    if json {
                        document(
                            "barge.root-evicted/1",
                            vec![("action", json!("evicted")), ("eviction", json!(eviction))],
                        )?;
                    } else {
                        println!(
                            "evicted {} from {} ({}), trash at {}",
                            eviction.id,
                            eviction.area,
                            describe_size(eviction.bytes, eviction.bytes_approximate),
                            eviction.trash_path.display()
                        );
                        if let Some(reason) = &eviction.kept_reason {
                            println!("trash kept at {}: {reason}", eviction.trash_path.display());
                        }
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

pub(crate) fn finish(json: bool, schema: &str, area: &str, id: &str, action: &str) -> Result<()> {
    if json {
        document(
            schema,
            vec![
                ("area_key", json!(area)),
                ("id", json!(id)),
                ("action", json!(action)),
            ],
        )?;
    } else {
        println!("{action} {id} in {area}");
    }
    Ok(())
}
