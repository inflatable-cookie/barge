use std::path::Path;

use barge::{Barge, Error, ErrorKind, MoveApplyOptions, MovePlan, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::output::document;

pub(crate) fn move_resource(arguments: &[String], json: bool) -> Result<()> {
    if arguments.iter().any(|arg| arg == "--apply") {
        let options = Options::parse(
            arguments,
            &["plan"],
            &["apply", "confirm-stopped", "retarget-unrecorded"],
        )?;
        expect_no_positionals(&options)?;
        let path = options.required("plan")?;
        let source = if path == "-" {
            std::io::read_to_string(std::io::stdin().lock())
                .map_err(|e| Error::new(ErrorKind::Io, format!("cannot read plan: {e}")))?
        } else {
            std::fs::read_to_string(&path)
                .map_err(|e| Error::usage(format!("cannot read plan {path}: {e}")))?
        };
        let plan: MovePlan = serde_json::from_str(&source)
            .map_err(|e| Error::usage(format!("invalid move plan: {e}")))?;
        let report = Barge::open()?.apply_move(
            &plan,
            MoveApplyOptions {
                confirm_stopped: options.flag("confirm-stopped"),
                retarget_unrecorded: options.flag("retarget-unrecorded"),
            },
        )?;
        if json {
            document("barge.move/1", vec![("report", json!(report))])?;
        } else {
            println!(
                "moved {} -> {}",
                plan.source.display(),
                plan.destination.display()
            );
            for link in &plan.unrecorded_links {
                println!(
                    "retargeted unrecorded {} link {} -> {}",
                    link.location,
                    link.link.display(),
                    link.after.display()
                );
                println!("  record it with: {}", link.adopt);
            }
            println!(
                "doctor: {}",
                match &report.doctor {
                    Some(doctor) if doctor.ok => "ok".to_string(),
                    Some(_) => "issues found".to_string(),
                    None => format!(
                        "failed: {}",
                        report.doctor_error.as_deref().unwrap_or("unknown error")
                    ),
                }
            );
        }
        return Ok(());
    }
    let options = Options::parse(arguments, &["item", "into"], &["plan"])?;
    if !options.flag("plan") {
        return Err(Error::usage("move requires --plan or --apply --plan FILE"));
    }
    let barge = Barge::open_read_only()?;
    let plan = if let Some(item) = options.get("item") {
        expect_no_positionals(&options)?;
        let into = options.required("into")?;
        let (class, sub) = into.split_once('/').unwrap_or((&into, ""));
        barge.plan_item_move(Path::new(&item), class.parse()?, sub)?
    } else {
        if options.get("into").is_some() {
            return Err(Error::usage("--into requires --item"));
        }
        let [class, path] = options.positionals.as_slice() else {
            return Err(Error::usage("move needs <class> <new-path>"));
        };
        barge.plan_class_move(class.parse()?, path)?
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&plan)
            .map_err(|e| Error::unknown_state(format!("cannot encode move plan: {e}")))?
    );
    Ok(())
}
