use barge::{Barge, Result};
use serde_json::json;

use crate::args::Options;
use crate::human::describe_size;
use crate::output::document;

pub(crate) fn survey(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &[], &["bytes", "no-bytes"])?;
    let name = options.single("survey", "volume")?;
    let items = Barge::open_read_only()?.survey(&name, options.flag("bytes"))?;
    if json {
        document(
            "barge.survey/1",
            vec![("volume_name", json!(name)), ("items", json!(items))],
        )?;
    } else {
        for item in items {
            println!(
                "{}  {}{}",
                item.path.display(),
                item.classification,
                item.bytes
                    .map(|b| format!("  {}", describe_size(b, item.bytes_approximate)))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}
