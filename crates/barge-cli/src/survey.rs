use super::*;

pub(super) fn survey(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &[], &["no-bytes"])?;
    let name = options.single("survey", "volume")?;
    let items = Barge::open_read_only()?.survey(&name, !options.flag("no-bytes"))?;
    if json {
        document(
            "barge.survey/1",
            vec![("volume", json!(name)), ("items", json!(items))],
        )?;
    } else {
        for item in items {
            println!(
                "{}  {}{}",
                item.path.display(),
                item.classification,
                item.bytes
                    .map(|b| format!("  {}", format_size(b)))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}
