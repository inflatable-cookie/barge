use std::process::ExitCode;

use barge::{Barge, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::human::print_doctor;
use crate::output::document;

pub(crate) fn doctor(arguments: &[String], json: bool) -> Result<ExitCode> {
    let options = Options::parse(arguments, &[], &[])?;
    expect_no_positionals(&options)?;
    let report = Barge::open_read_only()?.doctor()?;
    if json {
        let mut fields = vec![
            ("ok", json!(report.ok)),
            ("volume_reports", json!(report.volume_reports)),
            ("binding_reports", json!(report.binding_reports)),
            ("misplaced_areas", json!(report.misplaced_areas)),
        ];
        if !report.worktree_clone_problems.is_empty() {
            fields.push((
                "worktree_clone_problems",
                json!(report.worktree_clone_problems),
            ));
        }
        document("barge.doctor/1", fields)?;
    } else {
        print_doctor(&report);
    }
    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
