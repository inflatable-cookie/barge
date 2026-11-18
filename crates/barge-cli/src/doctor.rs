use super::*;

pub(super) fn doctor(arguments: &[String], json: bool) -> Result<ExitCode> {
    let options = Options::parse(arguments, &[], &[])?;
    expect_no_positionals(&options)?;
    let report = Barge::open_read_only()?.doctor()?;
    if json {
        document(
            "barge.doctor/1",
            vec![
                ("ok", json!(report.ok)),
                ("volumes", json!(report.volumes)),
                ("bindings", json!(report.bindings)),
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
