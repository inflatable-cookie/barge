use super::*;

pub(super) fn bind(arguments: &[String], json: bool) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("bind needs list, plan, apply, adopt or undo"));
    };
    let rest = &arguments[1..];
    match command {
        "list" => {
            let options = Options::parse(rest, &[], &[])?;
            expect_no_positionals(&options)?;
            let bindings = Barge::open_read_only()?.bindings().list()?;
            if json {
                document("barge.binding-list/1", vec![("bindings", json!(bindings))])?;
            } else if bindings.is_empty() {
                println!("no bindings recorded");
            } else {
                for record in &bindings {
                    println!(
                        "{} {} {} at {} -> {}",
                        record.consumer,
                        record.class.name(),
                        record.kind.name(),
                        record.link_path().display(),
                        record.applied.value.display()
                    );
                }
            }
            Ok(())
        }
        "plan" => {
            let options = Options::parse(rest, &["consumer", "class", "link", "to"], &[])?;
            expect_no_positionals(&options)?;
            let binding = binding_from_options(&options)?;
            let plan = Barge::open_read_only()?.bindings().plan(&binding)?;
            if json {
                document("barge.bind-plan/1", vec![("plan", json!(plan))])?;
            } else {
                println!(
                    "link {} -> {}",
                    plan.link.display(),
                    link_destination(&plan)
                );
                println!("  before: {}", plan.before);
                println!("  after: {}", plan.after);
                if plan.replace {
                    println!("  replace: yes (pass --replace to preserve the existing entry)");
                }
                println!(
                    "apply with: barge bind apply --plan <file holding the --json plan document>"
                );
            }
            Ok(())
        }
        "apply" => {
            // The reviewed plan document is the input: it names the binding, the
            // entry it observed and the destination it applied, so what was
            // reviewed is exactly what is enforced.
            let options = Options::parse(rest, &["plan"], &["replace"])?;
            expect_no_positionals(&options)?;
            let plan = read_plan(&options.required("plan")?)?;
            let binding = binding_for_plan(&plan)?;
            let record = Barge::open()?.bindings().apply(
                &binding,
                &plan,
                ApplyOptions {
                    replace: options.flag("replace"),
                },
            )?;
            if json {
                document(
                    "barge.binding/1",
                    vec![("action", json!("applied")), ("binding", json!(record))],
                )?;
            } else {
                println!(
                    "applied {} {} at {} -> {} (was {})",
                    record.consumer,
                    record.class.name(),
                    record.link_path().display(),
                    record.applied.value.display(),
                    record.prior
                );
            }
            Ok(())
        }
        "adopt" => {
            let options = Options::parse(rest, &["consumer", "class", "link", "to"], &[])?;
            expect_no_positionals(&options)?;
            let binding = binding_from_options(&options)?;
            let record = Barge::open()?.bindings().adopt(&binding)?;
            if json {
                document(
                    "barge.binding/1",
                    vec![("action", json!("adopted")), ("binding", json!(record))],
                )?;
            } else {
                println!(
                    "adopted {} {} at {} -> {}",
                    record.consumer,
                    record.class.name(),
                    record.link_path().display(),
                    record.applied.value.display()
                );
            }
            Ok(())
        }
        "undo" => {
            let options = Options::parse(rest, &[], &[])?;
            let consumer = options.single("bind undo", "consumer")?;
            let record = Barge::open()?.bindings().undo(&consumer)?;
            if json {
                document(
                    "barge.binding/1",
                    vec![("action", json!("undone")), ("binding", json!(record))],
                )?;
            } else {
                println!(
                    "undid {} {} at {} (restored {})",
                    record.consumer,
                    record.class.name(),
                    record.link_path().display(),
                    record.prior
                );
            }
            Ok(())
        }
        other => Err(Error::usage(format!(
            "unknown bind command {other:?}; use list, plan, apply, adopt or undo"
        ))),
    }
}

pub(super) fn binding_from_options(options: &Options) -> Result<Binding> {
    let consumer = options.required("consumer")?;
    let class = options.required("class")?.parse::<Class>()?;
    let link = options.required("link")?;
    let mut binding = Binding::link(consumer, class, link);
    if let Some(value) = options.get("to") {
        binding = binding.to(value);
    }
    Ok(binding)
}

pub(super) fn binding_for_plan(plan: &barge::LinkPlan) -> Result<Binding> {
    let destination = match &plan.after {
        barge::Observed::Link { destination } => destination.clone(),
        other => {
            return Err(Error::usage(format!(
                "the plan's after is {other}, not a link"
            )))
        }
    };
    Ok(Binding::link(plan.consumer.clone(), plan.class, plan.link.clone()).to(destination))
}

/// Read the reviewed plan document `bind plan --json` printed, from a file or
/// `-` for standard input. A document with another schema is refused.
pub(super) fn read_plan(path: &str) -> Result<barge::LinkPlan> {
    let source = if path == "-" {
        std::io::read_to_string(std::io::stdin().lock()).map_err(|source| {
            Error::new(
                ErrorKind::Io,
                format!("cannot read the plan from stdin: {source}"),
            )
        })?
    } else {
        std::fs::read_to_string(path)
            .map_err(|source| Error::usage(format!("cannot read plan {path}: {source}")))?
    };
    let document: Value = serde_json::from_str(&source)
        .map_err(|source| Error::usage(format!("{path} is not a JSON document: {source}")))?;
    let schema = document
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema != "barge.bind-plan/1" {
        return Err(Error::usage(format!(
            "{path} is {schema:?}, not a barge.bind-plan/1 document"
        )));
    }
    let plan = document
        .get("data")
        .and_then(|data| data.get("plan"))
        .ok_or_else(|| Error::usage(format!("{path} holds no plan")))?;
    serde_json::from_value(plan.clone())
        .map_err(|source| Error::usage(format!("{path} does not hold a plan: {source}")))
}

pub(super) fn link_destination(plan: &barge::LinkPlan) -> String {
    match &plan.after {
        barge::Observed::Link { destination } => destination.display().to_string(),
        other => other.to_string(),
    }
}
