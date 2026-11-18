use super::*;

pub(super) fn area(arguments: &[String], json: bool) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("area needs declare or list"));
    };
    let rest = &arguments[1..];
    match subcommand {
        "declare" => {
            let options = Options::parse(
                rest,
                &[
                    "namespace",
                    "name",
                    "volume",
                    "folder",
                    "absent",
                    "soft-cap",
                    "counts-toward-reserve",
                ],
                &[],
            )?;
            expect_no_positionals(&options)?;
            let namespace = options.required("namespace")?;
            let name = options.required("name")?;
            let volume = options.required("volume")?;
            let mut spec = AreaSpec::new(namespace, name, volume);
            if let Some(folder) = options.get("folder") {
                spec = spec.folder(folder);
            }
            if let Some(absent) = options.get("absent") {
                spec = spec.absent(parse_absent(&absent)?);
            }
            if let Some(soft_cap) = options.get("soft-cap") {
                spec = spec.soft_cap_bytes(Some(parse_size(&soft_cap)?));
            }
            if let Some(counts) = options.get("counts-toward-reserve") {
                spec = spec.counts_toward_reserve(parse_bool("counts-toward-reserve", &counts)?);
            }
            let barge = Barge::open()?;
            let area = barge.area(spec)?;
            let status = area.status()?;
            if json {
                document("barge.area-declared/1", vec![("area", json!(status))])?;
            } else {
                println!("declared area {}", area_line(&status));
            }
            Ok(())
        }
        "list" => {
            let options = Options::parse(rest, &["volume"], &[])?;
            expect_no_positionals(&options)?;
            let barge = Barge::open_read_only()?;
            let areas = barge.area_statuses(options.get("volume").as_deref())?;
            if json {
                document("barge.areas/1", vec![("areas", json!(areas))])?;
            } else if areas.is_empty() {
                println!("no areas declared");
            } else {
                for area in &areas {
                    println!("{}", area_line(area));
                }
            }
            Ok(())
        }
        other => Err(Error::usage(format!(
            "unknown area command {other:?}; use declare or list"
        ))),
    }
}
