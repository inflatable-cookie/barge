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

pub(super) fn print_doctor(report: &DoctorReport) {
    for volume in &report.volumes {
        println!(
            "volume {} at {}: {}",
            volume.name,
            volume.mount.display(),
            describe_state(&volume.state)
        );
        if let Some(layout) = &volume.layout {
            println!(
                "  layout: {}",
                if layout.agrees {
                    "the two copies agree"
                } else {
                    "the volume record disagrees with the account"
                }
            );
            for class in &layout.classes {
                println!(
                    "  class {} at {}: {}",
                    class.class.name(),
                    class.path.display(),
                    if class.exists { "present" } else { "missing" }
                );
            }
            for path in &layout.unknown {
                println!("  unknown {}", path.display());
            }
        }
    }
    for binding in &report.bindings {
        println!(
            "binding {} {} {} at {}: {}",
            binding.consumer,
            binding.class.name(),
            binding.kind.name(),
            binding.link.display(),
            describe_binding(&binding.state)
        );
    }
    println!(
        "doctor: {}",
        if report.ok {
            "ok".to_string()
        } else {
            format!("{} problems", report.problems())
        }
    );
}

pub(super) fn describe_binding(state: &BindingState) -> String {
    match state {
        BindingState::Ok => "ok".to_string(),
        BindingState::Missing => "missing: nothing is at the target".to_string(),
        BindingState::Unavailable { reason } => format!("unavailable: {reason}"),
        BindingState::Drifted {
            observed,
            expected,
            reason,
        } => format!(
            "drifted ({reason}): expected {}, observed {}",
            expected.display(),
            observed
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "nothing".to_string())
        ),
    }
}
