//! Space policy: a volume's reserve triggers pruning, an area's soft cap counts
//! only its own bytes, and only the floor refuses work.
//!
//! Eviction is least-recently-used first and only over idle, unpinned roots. A
//! root that is held or pinned keeps the run going, and is reported as the reason
//! the run stopped.

use std::collections::HashSet;

use serde::Serialize;

use crate::area::Area;
use crate::error::{Error, Result};
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
    pub reason: PruneReason,
}

/// One root a prune kept, with the reason it was not a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptRoot {
    pub area: String,
    pub id: String,
    pub label: String,
    pub bytes: u64,
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
    let volume = volume(core, volume_name)?;
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

    if !dry_run {
        for area in &areas {
            let sweep = area.sweep_trash()?;
            report.swept.extend(sweep.deleted);
            report.trash_kept.extend(sweep.kept);
        }
    }

    // Each area's soft cap counts only that area's own bytes.
    for area in &areas {
        let Some(cap) = area.record().soft_cap_bytes else {
            continue;
        };
        let listing = area.list()?;
        let mut bytes = listing.bytes();
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
            if evict(area, &info, PruneReason::SoftCap, dry_run, &mut report)?.is_none() {
                continue;
            }
            planned.insert((area.key().label(), info.id.clone()));
            bytes = bytes.saturating_sub(info.bytes);
        }
        if bytes > cap {
            keep_protected(area, &mut report, Some("the area is over its soft cap"));
        }
    }

    // The reserve: least-recently-used idle, unpinned roots on the volume, from
    // the areas that count toward it.
    let mut candidates: Vec<(Area, RootInfo)> = Vec::new();
    let mut protected: Vec<(Area, RootInfo)> = Vec::new();
    for area in &areas {
        if !area.record().counts_toward_reserve {
            continue;
        }
        for info in area.list()?.roots {
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
    let mut estimate = free_before.saturating_add(report.freed_bytes);
    for (area, info) in &candidates {
        if estimate >= target_bytes {
            break;
        }
        if planned.contains(&(area.key().label(), info.id.clone())) {
            continue;
        }
        if let Some(evicted) = evict(area, info, PruneReason::Reserve, dry_run, &mut report)? {
            estimate = estimate.saturating_add(evicted.bytes);
        }
    }
    if estimate < target_bytes {
        for (area, info) in &protected {
            report.kept.push(KeptRoot {
                area: area.key().label(),
                id: info.id.clone(),
                label: info.label.clone(),
                bytes: info.bytes,
                reason: if info.held {
                    KeepReason::Held
                } else {
                    KeepReason::Pinned
                },
                detail: Some("protected from prune".to_string()),
            });
        }
    }
    report.free_after_bytes = if dry_run {
        free_before
    } else {
        core.os
            .free_bytes(&mount)
            .unwrap_or(free_before.saturating_add(report.freed_bytes))
    };
    Ok(report)
}

fn evict(
    area: &Area,
    info: &RootInfo,
    reason: PruneReason,
    dry_run: bool,
    report: &mut PruneReport,
) -> Result<Option<EvictedRoot>> {
    if dry_run {
        let planned = EvictedRoot {
            area: area.key().label(),
            id: info.id.clone(),
            label: info.label.clone(),
            path: info.path.clone(),
            bytes: info.bytes,
            reason,
        };
        report.freed_bytes = report.freed_bytes.saturating_add(info.bytes);
        report.evicted.push(planned.clone());
        return Ok(Some(planned));
    }
    match area.evict(&info.id) {
        Ok(eviction) => {
            let evicted = EvictedRoot {
                area: area.key().label(),
                id: info.id.clone(),
                label: info.label.clone(),
                path: eviction.path,
                bytes: eviction.bytes,
                reason,
            };
            report.freed_bytes = report.freed_bytes.saturating_add(evicted.bytes);
            report.evicted.push(evicted.clone());
            Ok(Some(evicted))
        }
        Err(error) => {
            report.kept.push(KeptRoot {
                area: area.key().label(),
                id: info.id.clone(),
                label: info.label.clone(),
                bytes: info.bytes,
                reason: KeepReason::Refused,
                detail: Some(error.message().to_string()),
            });
            Ok(None)
        }
    }
}

fn keep_protected(area: &Area, report: &mut PruneReport, detail: Option<&str>) {
    let Ok(listing) = area.list() else {
        return;
    };
    for info in listing.roots {
        if info.idle_and_unpinned() {
            continue;
        }
        report.kept.push(KeptRoot {
            area: area.key().label(),
            id: info.id.clone(),
            label: info.label.clone(),
            bytes: info.bytes,
            reason: if info.held {
                KeepReason::Held
            } else {
                KeepReason::Pinned
            },
            detail: detail.map(str::to_string),
        });
    }
}

fn volume(core: &Core, name: &str) -> Result<Volume> {
    let spec = core
        .state
        .read_volumes()?
        .into_iter()
        .find(|spec| spec.name == name)
        .ok_or_else(|| Error::not_found(format!("volume {name} is not registered")))?;
    Ok(Volume::new(core.clone(), spec))
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
