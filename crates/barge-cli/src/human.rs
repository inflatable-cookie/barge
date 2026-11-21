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
        "{} the scratch file {}",
        match report.action {
            InitAction::Inserted => "wrote",
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
    for warning in &report.warnings {
        eprintln!("barge: {warning}");
    }
}

pub(crate) fn print_init_check(check: &InitCheck) {
    if check.is_current() {
        println!("the scratch file {} is current", check.path.display());
    } else {
        for line in &check.diff {
            println!("{line}");
        }
        if check.state != BlockState::Current {
            eprintln!(
                "barge: the scratch file {} is {}; run `barge init`",
                check.path.display(),
                check.state.name()
            );
        }
    }
    for warning in &check.warnings {
        eprintln!("barge: {warning}");
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

/// The current time in Unix milliseconds, for the human machine output.
pub(crate) fn now_unix_ms() -> u64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
}

/// A machine's observed state, as its age: "12 s ago", "3 min ago", "2 h ago",
/// "4 d ago". A timestamp ahead of `now` (clock skew) reads as "0 s ago".
pub(crate) fn age_text(now_ms: u64, then_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(then_ms) / 1000;
    match seconds {
        s if s < 60 => format!("{s} s ago"),
        s if s < 3_600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3_600),
        s => format!("{} d ago", s / 86_400),
    }
}

/// How long until a pending action fires: "in 9 s", rounding up, never negative.
pub(crate) fn until_text(now_ms: u64, deadline_ms: u64) -> String {
    format!("in {} s", deadline_ms.saturating_sub(now_ms).div_ceil(1000))
}

/// A Unix-millisecond instant in the machine's local time zone, as
/// `2026-11-19 14:03:22`.
pub(crate) fn local_clock(unix_ms: u64) -> String {
    let unix_s = i64::try_from(unix_ms / 1000).unwrap_or(i64::MAX);
    clock_text(unix_s, local_offset_seconds(unix_s))
}

/// The date and time at `unix_s` shifted by `offset_s` seconds east of UTC.
pub(crate) fn clock_text(unix_s: i64, offset_s: i64) -> String {
    let local = unix_s.saturating_add(offset_s);
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    let seconds = local.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian (year, month,
/// day) of a count of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The machine's time-zone offset at `unix_s`, in seconds east of UTC. A
/// conversion the system refuses reads as UTC.
fn local_offset_seconds(unix_s: i64) -> i64 {
    // time_t is 64 bits on every target Barge builds for.
    let time: libc::time_t = unix_s;
    // SAFETY: a zeroed `tm` is valid, since its fields are integers and a null pointer.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `time` and `tm` are live for the call, and localtime_r writes only into `tm`.
    let converted = unsafe { libc::localtime_r(&time, &mut tm) };
    if converted.is_null() {
        0
    } else {
        tm.tm_gmtoff
    }
}

#[cfg(test)]
mod tests {
    use super::{age_text, clock_text, until_text};

    #[test]
    fn clocks_show_the_date_and_time_with_the_offset_applied() {
        assert_eq!(clock_text(0, 0), "1970-01-01 00:00:00");
        assert_eq!(clock_text(1_700_000_000, 0), "2023-11-14 22:13:20");
        assert_eq!(clock_text(1_700_000_000, 3_600), "2023-11-14 23:13:20");
        assert_eq!(clock_text(0, -1), "1969-12-31 23:59:59");
        // 2000 was a leap year: 29 February is a day of its own.
        assert_eq!(clock_text(951_782_400, 0), "2000-02-29 00:00:00");
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        let now = 1_000_000_000;
        assert_eq!(age_text(now, now - 12_000), "12 s ago");
        assert_eq!(age_text(now, now - 180_000), "3 min ago");
        assert_eq!(age_text(now, now - 7_200_000), "2 h ago");
        assert_eq!(age_text(now, now - 4 * 86_400_000), "4 d ago");
        assert_eq!(age_text(now, now + 5_000), "0 s ago");
    }

    #[test]
    fn countdowns_round_up_and_stop_at_zero() {
        assert_eq!(until_text(1_000, 10_000), "in 9 s");
        assert_eq!(until_text(1_000, 1_001), "in 1 s");
        assert_eq!(until_text(5_000, 1_000), "in 0 s");
    }
}
