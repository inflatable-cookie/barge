//! `barge status`: every registered volume, its states and free space.

use barge::{Barge, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::human::{area_line, volume_line};
use crate::output::document;

/// Print the account's status: read-only, so it never creates state.
pub(crate) fn status(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &[], &[])?;
    expect_no_positionals(&options)?;
    let barge = Barge::open_read_only()?;
    let status = barge.status()?;
    if json {
        document("barge.status/1", vec![("volumes", json!(status))])?;
    } else {
        for volume in &status {
            println!("{}", volume_line(volume));
            for area in &volume.areas {
                println!("  {}", area_line(area));
            }
        }
    }
    Ok(())
}
