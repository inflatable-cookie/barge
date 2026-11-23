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
        document(
            "barge.doctor/1",
            vec![
                ("ok", json!(report.ok)),
                ("volume_reports", json!(report.volume_reports)),
                ("binding_reports", json!(report.binding_reports)),
                ("misplaced_areas", json!(report.misplaced_areas)),
            ],
        )?;
    } else {
        print_doctor(&report);
    }
    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
