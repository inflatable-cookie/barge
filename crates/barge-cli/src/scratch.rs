use std::time::Duration;

use barge::{Barge, Error, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::human::print_sweep;
use crate::output::document;

pub(crate) fn scratch(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("scratch needs new or sweep"));
    };
    let rest = &arguments[1..];
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
            let barge = Barge::open()?;
            let scratch = barge.scratch(&volume, &project)?;
            let root = scratch.create(&label, Duration::from_secs(ttl))?;
            if json {
                document(
                    "barge.scratch/1",
                    vec![
                        ("area_key", json!(scratch.area().key().label())),
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
            let barge = Barge::open()?;
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
