use super::*;

pub(super) fn prune(arguments: &[String], json: bool) -> Result<()> {
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
