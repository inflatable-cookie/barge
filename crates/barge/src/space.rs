//! Space policy: a volume's reserve triggers pruning, an area's soft cap counts
//! only its own bytes, and only the floor refuses work.
//!
//! Eviction is least-recently-used first and only over idle, unpinned roots. A
//! root that is held or pinned keeps the run going, and is reported as the reason
//! the run stopped. An eviction the filesystem refuses is returned as an error
//! with its cause, never hidden as another policy refusal.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::area::Area;
use crate::error::{ErrorKind, Result};
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

    soft_cap_pass(&areas, dry_run, &mut report, &mut planned)?;
    reserve_pass(
        core,
        &mount,
        &areas,
        dry_run,
        target_bytes,
        &planned,
        &mut report,
    )?;
    Ok(report)
}

fn soft_cap_pass(
    areas: &[Area],
    dry_run: bool,
    report: &mut PruneReport,
    planned: &mut HashSet<(String, String)>,
) -> Result<()> {
    // Each area's soft cap counts only that area's own bytes.
    for area in areas {
        let Some(cap) = area.record().soft_cap_bytes else {
            continue;
        };
        let listing = area.list_without_bytes()?;
        let mut sizes = HashMap::with_capacity(listing.roots.len());
        let mut bytes = 0_u64;
        for info in &listing.roots {
            let size = area.recorded_or_measure_bytes(info, !dry_run);
            bytes = bytes.saturating_add(size.bytes);
            sizes.insert(info.id.clone(), size.bytes);
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
            if evict(area, &info, PruneReason::SoftCap, dry_run, report)?.is_none() {
                continue;
            }
            planned.insert((area.key().label(), info.id.clone()));
            let size = sizes.get(&info.id).copied().unwrap_or(0);
            bytes = bytes.saturating_sub(size);
        }
        if bytes > cap {
            keep_protected(area, report, Some("the area is over its soft cap"))?;
        }
    }

    Ok(())
}

fn reserve_pass(
    core: &Core,
    mount: &std::path::Path,
    areas: &[Area],
    dry_run: bool,
    target_bytes: u64,
    planned: &HashSet<(String, String)>,
    report: &mut PruneReport,
) -> Result<()> {
    let current_free = if dry_run {
        report.free_before_bytes.saturating_add(report.freed_bytes)
    } else {
        core.os.free_bytes(mount)?
    };
    if current_free >= target_bytes {
        report.free_after_bytes = if dry_run {
            report.free_before_bytes
        } else {
            current_free
        };
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
        let free = if dry_run {
            report.free_before_bytes.saturating_add(report.freed_bytes)
        } else {
            core.os.free_bytes(mount)?
        };
        if free >= target_bytes {
            break;
        }
        if planned.contains(&(area.key().label(), info.id.clone())) {
            continue;
        }
        evict(area, info, PruneReason::Reserve, dry_run, report)?;
    }
    let free_after = if dry_run {
        report.free_before_bytes
    } else {
        core.os.free_bytes(mount)?
    };
    if (dry_run && report.free_before_bytes.saturating_add(report.freed_bytes) < target_bytes)
        || (!dry_run && free_after < target_bytes)
    {
        for (area, info) in &protected {
            let size = area.recorded_or_measure_bytes(info, false);
            report.kept.push(KeptRoot {
                area: area.key().label(),
                id: info.id.clone(),
                label: info.label.clone(),
                bytes: size.bytes,
                bytes_approximate: size.approximate,
                reason: if info.held {
                    KeepReason::Held
                } else {
                    KeepReason::Pinned
                },
                detail: Some("protected from prune".to_string()),
            });
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
    report: &mut PruneReport,
) -> Result<Option<EvictedRoot>> {
    if dry_run {
        let size = crate::root::tree_bytes(&info.path);
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
    match area.evict(&info.id) {
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
            let size = area.recorded_or_measure_bytes(info, false);
            let kept = KeptRoot {
                area: area.key().label(),
                id: info.id.clone(),
                label: info.label.clone(),
                bytes: size.bytes,
                bytes_approximate: size.approximate,
                reason: KeepReason::Refused,
                detail: Some(error.message().to_string()),
            };
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
        report.kept.push(KeptRoot {
            area: area.key().label(),
            id: info.id.clone(),
            label: info.label.clone(),
            bytes: size.bytes,
            bytes_approximate: size.approximate,
            reason: if info.held {
                KeepReason::Held
            } else {
                KeepReason::Pinned
            },
            detail: detail.map(str::to_string),
        });
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
