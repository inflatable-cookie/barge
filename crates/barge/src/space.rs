//! Space policy: a volume's reserve triggers pruning, an area's soft cap counts
//! only its own bytes, and only the floor refuses work.
//!
//! A soft cap is a budget on the area, so it counts the area's roots wherever
//! they live: on the volume or in its declared fallback on the internal drive.
//! The reserve is about one volume's free space, so only roots on that volume
//! count toward it, and only bytes freed on that volume are credited to it.
//!
//! Eviction is least-recently-used first and only over idle, unpinned roots. A
//! root that is held or pinned keeps the run going, and is reported as the reason
//! the run stopped. An eviction the filesystem refuses is returned as an error
//! with its cause, never hidden as another policy refusal.
//!
//! APFS can report freed space late, so a reserve pass counts the bytes it has
//! evicted and stops once that reaches the target. Free space is asked again only
//! once, at the end of the pass, for the report.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::area::Area;
use crate::error::{ErrorKind, Result};
use crate::os::TreeSize;
use crate::root::RootInfo;
use crate::volume::Volume;
use crate::Core;

/// What a volume's space policy looks like right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Space {
    pub free_bytes: u64,
    pub reserve_bytes: u64,
    pub floor_bytes: u64,
}

/// Why a root was evicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneReason {
    /// The area was over its soft cap.
    SoftCap,
    /// The volume was below its reserve.
    Reserve,
}

/// Why a root was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepReason {
    Held,
    Pinned,
    /// A policy refused the eviction when it was attempted.
    Refused,
}

/// One root a prune evicted, or would evict in a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvictedRoot {
    pub area: String,
    pub id: String,
    pub label: String,
    pub path: std::path::PathBuf,
    pub bytes: u64,
    /// True when the tree size could not be measured exactly.
    pub bytes_approximate: bool,
    pub reason: PruneReason,
}

/// One root a prune kept, with the reason it was not a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptRoot {
    pub area: String,
    pub id: String,
    pub label: String,
    pub bytes: u64,
    /// True when the tree size could not be measured exactly.
    pub bytes_approximate: bool,
    pub reason: KeepReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl KeptRoot {
    /// One kept root, with the size the policy counted for it.
    fn new(
        area: &Area,
        info: &RootInfo,
        reason: KeepReason,
        size: TreeSize,
        detail: Option<String>,
    ) -> Self {
        Self {
            area: area.key().label(),
            id: info.id.clone(),
            label: info.label.clone(),
            bytes: size.bytes,
            bytes_approximate: size.approximate,
            reason,
            detail,
        }
    }
}

/// Why a root that is not a candidate was kept: something holds it, or a pin
/// protects it.
fn protected_reason(info: &RootInfo) -> KeepReason {
    if info.held {
        KeepReason::Held
    } else {
        KeepReason::Pinned
    }
}

/// The result of a prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    pub volume: String,
    pub dry_run: bool,
    pub free_before_bytes: u64,
    pub free_after_bytes: u64,
    pub freed_bytes: u64,
    pub reserve_bytes: u64,
    /// The free-space target this run worked toward: the reserve, or the floor
    /// for an admission.
    pub target_bytes: u64,
    pub floor_bytes: u64,
    pub evicted: Vec<EvictedRoot>,
    pub kept: Vec<KeptRoot>,
    pub swept: Vec<SweptRoot>,
    pub trash_kept: Vec<SkippedRoot>,
}

impl PruneReport {
    /// Every root the run removed, or would remove in a dry run.
    pub fn evicted_bytes(&self) -> u64 {
        self.evicted.iter().map(|root| root.bytes).sum()
    }
}

/// A trash entry deleted by an explicit sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SweptRoot {
    pub path: std::path::PathBuf,
    pub bytes: u64,
}

/// A trash entry a sweep left in place, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedRoot {
    pub path: std::path::PathBuf,
    pub reason: String,
}

/// The result of sweeping trash or expired scratch roots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub deleted: Vec<SweptRoot>,
    pub kept: Vec<SkippedRoot>,
}

impl SweepReport {
    pub fn deleted_bytes(&self) -> u64 {
        self.deleted.iter().map(|root| root.bytes).sum()
    }
}

/// Prune a volume: sweep its trash, enforce each area's soft cap, then return
/// free space above the volume's reserve by evicting least-recently-used idle
/// unpinned roots.
/// `target_bytes` overrides the reserve: `prune` leaves it unset, while `admit`
/// passes the floor, so an admission evicts only what the floor needs.
pub(crate) fn prune(
    core: &Core,
    volume_name: &str,
    dry_run: bool,
    target_bytes: Option<u64>,
) -> Result<PruneReport> {
    let volume = core.volume(volume_name)?;
    let mount = volume.require_present()?;
    let spec = volume.spec().clone();
    let free_before = core.os.free_bytes(&mount)?;
    let areas = areas(core, &volume)?;

    let target_bytes = target_bytes.unwrap_or(spec.reserve_bytes);
    let mut report = PruneReport {
        volume: volume_name.to_string(),
        dry_run,
        free_before_bytes: free_before,
        free_after_bytes: free_before,
        freed_bytes: 0,
        reserve_bytes: spec.reserve_bytes,
        target_bytes,
        floor_bytes: spec.floor_bytes,
        evicted: Vec::new(),
        kept: Vec::new(),
        swept: Vec::new(),
        trash_kept: Vec::new(),
    };
    let mut planned: HashSet<(String, String)> = HashSet::new();

    if free_before >= target_bytes
        && areas
            .iter()
            .all(|area| area.record().soft_cap_bytes.is_none())
    {
        return Ok(report);
    }

    if !dry_run {
        for area in &areas {
            let sweep = area.sweep_trash()?;
            report.swept.extend(sweep.deleted);
            report.trash_kept.extend(sweep.kept);
        }
    }

    // Bytes freed on the volume itself, which the reserve pass counts as it
    // evicts instead of re-reading a filesystem that may report them late.
    let mut freed_on_volume = 0_u64;
    soft_cap_pass(
        &areas,
        dry_run,
        &mut report,
        &mut planned,
        &mut freed_on_volume,
    )?;
    reserve_pass(
        core,
        &mount,
        &areas,
        Target {
            bytes: target_bytes,
            dry_run,
        },
        &planned,
        &mut report,
        freed_on_volume,
    )?;
    Ok(report)
}

fn soft_cap_pass(
    areas: &[Area],
    dry_run: bool,
    report: &mut PruneReport,
    planned: &mut HashSet<(String, String)>,
    freed_on_volume: &mut u64,
) -> Result<()> {
    // Each area's soft cap counts only that area's own bytes, on the volume or in
    // its declared fallback: the cap is a budget on the area, not on one drive.
    for area in areas {
        let Some(cap) = area.record().soft_cap_bytes else {
            continue;
        };
        let listing = area.list_without_bytes()?;
        let mut sizes: HashMap<String, TreeSize> = HashMap::with_capacity(listing.roots.len());
        let mut bytes = 0_u64;
        for info in &listing.roots {
            let size = area.recorded_or_measure_bytes(info, !dry_run);
            bytes = bytes.saturating_add(size.bytes);
            sizes.insert(info.id.clone(), size);
        }
        if bytes <= cap {
            continue;
        }
        let mut candidates: Vec<RootInfo> = listing
            .roots
            .into_iter()
            .filter(RootInfo::idle_and_unpinned)
            .collect();
        candidates.sort_by(|left, right| {
            (left.last_used_unix_ms, &left.id).cmp(&(right.last_used_unix_ms, &right.id))
        });
        for info in candidates {
            if bytes <= cap {
                break;
            }
            // A dry run plans with the size this pass already measured instead of
            // walking the root a second time.
            let size = sizes.get(&info.id).copied();
            let freed_before = report.freed_bytes;
            if evict(area, &info, PruneReason::SoftCap, dry_run, size, report)?.is_none() {
                continue;
            }
            // Only a fallback root leaves the volume's free space alone.
            if info.on_volume {
                *freed_on_volume =
                    freed_on_volume.saturating_add(report.freed_bytes.saturating_sub(freed_before));
            }
            planned.insert((area.key().label(), info.id.clone()));
            bytes = bytes.saturating_sub(size.map_or(0, |size| size.bytes));
        }
        if bytes > cap {
            keep_protected(area, report, Some("the area is over its soft cap"))?;
        }
    }

    Ok(())
}

/// What a reserve pass works toward, and whether it reports a prediction.
#[derive(Debug, Clone, Copy)]
struct Target {
    bytes: u64,
    dry_run: bool,
}

fn reserve_pass(
    core: &Core,
    mount: &Path,
    areas: &[Area],
    target: Target,
    planned: &HashSet<(String, String)>,
    report: &mut PruneReport,
    freed_on_volume: u64,
) -> Result<()> {
    // APFS can report freed space late, so count the bytes each eviction freed
    // instead of asking the filesystem after every root; it is asked once, at the
    // end, for the report.
    let mut free = report.free_before_bytes.saturating_add(freed_on_volume);
    if free >= target.bytes {
        report.free_after_bytes = free;
        return Ok(());
    }

    // The reserve: least-recently-used idle, unpinned roots on the volume, from
    // the areas that count toward it.
    let mut candidates: Vec<(Area, RootInfo)> = Vec::new();
    let mut protected: Vec<(Area, RootInfo)> = Vec::new();
    for area in areas {
        if !area.record().counts_toward_reserve {
            continue;
        }
        for info in area.list_without_bytes()?.roots {
            if !info.on_volume {
                continue;
            }
            if info.idle_and_unpinned() {
                candidates.push((area.clone(), info));
            } else {
                protected.push((area.clone(), info));
            }
        }
    }
    candidates.sort_by(|left, right| {
        (left.1.last_used_unix_ms, &left.1.id).cmp(&(right.1.last_used_unix_ms, &right.1.id))
    });
    for (area, info) in &candidates {
        if free >= target.bytes {
            break;
        }
        if planned.contains(&(area.key().label(), info.id.clone())) {
            continue;
        }
        let freed_before = report.freed_bytes;
        evict(
            area,
            info,
            PruneReason::Reserve,
            target.dry_run,
            None,
            report,
        )?;
        free = free.saturating_add(report.freed_bytes.saturating_sub(freed_before));
    }
    let free_after = if target.dry_run {
        free
    } else {
        core.os.free_bytes(mount)?
    };
    if free_after < target.bytes {
        for (area, info) in &protected {
            let size = area.recorded_or_measure_bytes(info, false);
            report.kept.push(KeptRoot::new(
                area,
                info,
                protected_reason(info),
                size,
                Some("protected from prune".to_string()),
            ));
        }
    }
    report.free_after_bytes = free_after;
    Ok(())
}

fn evict(
    area: &Area,
    info: &RootInfo,
    reason: PruneReason,
    dry_run: bool,
    size: Option<TreeSize>,
    report: &mut PruneReport,
) -> Result<Option<EvictedRoot>> {
    if dry_run {
        let size = size.unwrap_or_else(|| crate::root::tree_bytes(&info.path));
        let planned = EvictedRoot {
            area: area.key().label(),
            id: info.id.clone(),
            label: info.label.clone(),
            path: info.path.clone(),
            bytes: size.bytes,
            bytes_approximate: size.approximate,
            reason,
        };
        report.freed_bytes = report.freed_bytes.saturating_add(planned.bytes);
        report.evicted.push(planned.clone());
        return Ok(Some(planned));
    }
    match area.evict_listed(info) {
        Ok(eviction) => {
            if let Some(kept_reason) = &eviction.kept_reason {
                report.trash_kept.push(SkippedRoot {
                    path: eviction.trash_path.clone(),
                    reason: kept_reason.clone(),
                });
            }
            let evicted = EvictedRoot {
                area: area.key().label(),
                id: info.id.clone(),
                label: info.label.clone(),
                path: eviction.path,
                bytes: eviction.bytes,
                bytes_approximate: eviction.bytes_approximate,
                reason,
            };
            if eviction.deleted {
                report.freed_bytes = report.freed_bytes.saturating_add(evicted.bytes);
            }
            report.evicted.push(evicted.clone());
            Ok(Some(evicted))
        }
        Err(error) => {
            let size = size.unwrap_or_else(|| area.recorded_or_measure_bytes(info, false));
            let kept = KeptRoot::new(
                area,
                info,
                KeepReason::Refused,
                size,
                Some(error.message().to_string()),
            );
            match error.kind() {
                // A policy refusal keeps the root and the run carries on.
                ErrorKind::Refused | ErrorKind::NotFound => {
                    report.kept.push(kept);
                    Ok(None)
                }
                // A broken filesystem is never reported as a refusal.
                _ => Err(error),
            }
        }
    }
}

fn keep_protected(area: &Area, report: &mut PruneReport, detail: Option<&str>) -> Result<()> {
    let listing = area.list_without_bytes()?;
    for info in &listing.roots {
        if info.idle_and_unpinned() {
            continue;
        }
        let size = area.recorded_or_measure_bytes(info, false);
        report.kept.push(KeptRoot::new(
            area,
            info,
            protected_reason(info),
            size,
            detail.map(str::to_string),
        ));
    }
    Ok(())
}

fn areas(core: &Core, volume: &Volume) -> Result<Vec<Area>> {
    let mut areas = Vec::new();
    for record in core.state.read_areas()? {
        if record.volume != volume.name() {
            continue;
        }
        areas.push(Area::new(core.clone(), record, volume.clone()));
    }
    areas.sort_by_key(|area| area.key());
    Ok(areas)
}
