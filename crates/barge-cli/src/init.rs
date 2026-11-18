use std::path::PathBuf;
use std::process::ExitCode;

use barge::{Barge, Error, ErrorKind, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::human::{print_init, print_init_check};
use crate::output::document;

/// `barge init`: write the managed AGENTS.md block, or report it with `--check`.
pub(crate) fn init(arguments: &[String], json: bool) -> Result<ExitCode> {
    let options = Options::parse(arguments, &["project", "path"], &["check"])?;
    expect_no_positionals(&options)?;
    let repository = match options.get("path") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|source| {
            Error::new(
                ErrorKind::Io,
                format!("cannot read the current directory: {source}"),
            )
        })?,
    };
    let project = options.get("project");
    if options.flag("check") {
        let check = Barge::open_read_only()?.init_check(&repository, project.as_deref())?;
        if json {
            document(
                "barge.init-check/1",
                vec![
                    ("project", json!(check.project)),
                    ("path", json!(check.path)),
                    ("block_state", json!(check.state)),
                    ("scratch", json!(check.scratch)),
                    ("volume_name", json!(check.volume_name)),
                    ("volume_state", json!(check.volume_state)),
                    ("diff", json!(check.diff)),
                ],
            )?;
        } else {
            print_init_check(&check);
        }
        return Ok(if check.is_current() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let report = Barge::open()?.init(&repository, project.as_deref())?;
    if json {
        document(
            "barge.init/1",
            vec![
                ("project", json!(report.project)),
                ("path", json!(report.path)),
                ("action", json!(report.action)),
                ("scratch", json!(report.scratch)),
                ("volume_name", json!(report.volume_name)),
                ("volume_state", json!(report.volume_state)),
            ],
        )?;
    } else {
        print_init(&report);
    }
    Ok(ExitCode::SUCCESS)
}
