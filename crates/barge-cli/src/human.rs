use barge::{
    AreaStatus, BindingState, BlockState, DoctorReport, InitAction, InitCheck, InitReport,
    KeepReason, PruneReason, PruneReport, RootInfo, SweepReport, VolumeState, VolumeStatus,
};

pub(crate) fn print_prune(report: &PruneReport) {
    let prefix = if report.dry_run {
        "would evict"
    } else {
        "evicted"
    };
    for root in &report.evicted {
        println!(
            "{prefix} {} from {} ({}, {})",
            root.id,
            root.area,
            describe_size(root.bytes, root.bytes_approximate),
            match root.reason {
                PruneReason::SoftCap => "soft cap",
                PruneReason::Reserve => "reserve",
            }
        );
    }
    for root in &report.kept {
        println!(
            "kept {} from {} ({}, {})",
            root.id,
            root.area,
            describe_size(root.bytes, root.bytes_approximate),
            match root.reason {
                KeepReason::Held => "held",
                KeepReason::Pinned => "pinned",
                KeepReason::Refused => "refused",
            }
        );
    }
    for swept in &report.swept {
        println!(
            "swept trash {} ({})",
            swept.path.display(),
            format_size(swept.bytes)
        );
    }
    for kept in &report.trash_kept {
        println!("kept trash {}: {}", kept.path.display(), kept.reason);
    }
    println!(
        "volume {}: free {} -> {} (reserve {}, floor {})",
        report.volume,
        format_size(report.free_before_bytes),
        format_size(report.free_after_bytes),
        format_size(report.reserve_bytes),
        format_size(report.floor_bytes)
    );
}

pub(crate) fn print_sweep(report: &SweepReport) {
    for swept in &report.deleted {
        println!(
            "deleted {} ({})",
            swept.path.display(),
            format_size(swept.bytes)
        );
    }
    for kept in &report.kept {
        println!("kept {}: {}", kept.path.display(), kept.reason);
    }
    println!(
        "deleted {} in {} entries",
        format_size(report.deleted_bytes()),
        report.deleted.len()
    );
}

pub(crate) fn volume_line(volume: &VolumeStatus) -> String {
    format!(
        "{} at {}: {}{}",
        volume.spec.name,
        volume.spec.mount.display(),
        describe_state(&volume.state),
        volume
            .free_bytes
            .map(|free| format!(", free {}", format_size(free)))
            .unwrap_or_default()
    )
}

pub(crate) fn area_line(area: &AreaStatus) -> String {
    match (&area.location, &area.unavailable) {
        (Some(path), _) => format!(
            "area {}/{} at {}{} roots {}, {}{}",
            area.namespace,
            area.name,
            path.display(),
            if area.fallback { " (fallback)" } else { "" },
            area.roots,
            format_size(area.bytes),
            if area.stragglers > 0 {
                format!(", {} kept stragglers", area.stragglers)
            } else {
                String::new()
            }
        ),
        (None, Some(reason)) => format!(
            "area {}/{} unavailable: {reason}",
            area.namespace, area.name
        ),
        (None, None) => format!("area {}/{} unavailable", area.namespace, area.name),
    }
}

pub(crate) fn describe_state(state: &VolumeState) -> String {
    match state {
        VolumeState::Present => "present".to_string(),
        VolumeState::Absent => "absent".to_string(),
        VolumeState::Mismatch { registered, found } => {
            format!("mismatch: found {found}, registered {registered}")
        }
    }
}

pub(crate) fn describe_root(root: &RootInfo) -> String {
    let mut parts = Vec::new();
    parts.push(if root.held { "held" } else { "idle" }.to_string());
    if root.pinned {
        parts.push("pinned".to_string());
    }
    if let Some(expires) = root.expires_unix_ms {
        parts.push(format!("expires {expires}"));
    }
    parts.join(", ")
}

pub(crate) fn print_doctor(report: &DoctorReport) {
    for volume in &report.volume_reports {
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
    for binding in &report.binding_reports {
        println!(
            "binding {} {} {} at {}: {}",
            binding.consumer,
            binding.class.name(),
            binding.kind.name(),
            binding.path.display(),
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

pub(crate) fn describe_binding(state: &BindingState) -> String {
    match state {
        BindingState::Ok => "ok".to_string(),
        BindingState::Missing => "missing: nothing is at the target".to_string(),
        BindingState::Unavailable { reason } => format!("unavailable: {reason}"),
        BindingState::Stale { reason } => format!("stale: {reason}"),
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

pub(crate) fn print_init(report: &InitReport) {
    println!(
        "{} the barge workflow block in {}",
        match report.action {
            InitAction::Inserted => "inserted",
            InitAction::Replaced => "replaced",
            InitAction::Unchanged => "left unchanged",
        },
        report.path.display()
    );
    println!("  scratch: {}", report.scratch.display());
    if !report.volume_present() {
        eprintln!(
            "barge: volume {} is {}; rendered from the layout",
            report.volume_name,
            describe_state(&report.volume_state)
        );
    }
}

pub(crate) fn print_init_check(check: &InitCheck) {
    match check.state {
        BlockState::Current => println!(
            "the barge workflow block in {} is current",
            check.path.display()
        ),
        BlockState::Stale | BlockState::Missing => {
            for line in &check.diff {
                println!("{line}");
            }
            eprintln!(
                "barge: the barge workflow block in {} is {}; run `barge init`",
                check.path.display(),
                check.state.name()
            );
        }
    }
    if !check.volume_present() {
        eprintln!(
            "barge: volume {} is {}; rendered from the layout",
            check.volume_name,
            describe_state(&check.volume_state)
        );
    }
}

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A size a tree walk could not take exactly is shown as approximate.
pub(crate) fn describe_size(bytes: u64, approximate: bool) -> String {
    if approximate {
        format!("~{} (approximate)", format_size(bytes))
    } else {
        format_size(bytes)
    }
}
