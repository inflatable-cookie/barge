use barge::{Barge, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::human::print_prune;
use crate::output::document;

pub(crate) fn prune(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["volume"], &["dry-run"])?;
    expect_no_positionals(&options)?;
    let volume = options.required("volume")?;
    let dry_run = options.flag("dry-run");
    // A dry run is a preview: the library allows it on a read-only account, so
    // it must not create state either.
    let barge = if dry_run {
        Barge::open_read_only()?
    } else {
        Barge::open()?
    };
    let report = barge.prune(&volume, dry_run)?;
    if json {
        document("barge.prune/1", vec![("prune", json!(report))])?;
    } else {
        print_prune(&report);
    }
    Ok(())
}
