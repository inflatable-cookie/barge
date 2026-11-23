//! `barge doctor`: a read-only drift report over volumes, the layout and every
//! recorded binding. It changes nothing and works on a read-only account.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::bindings::{observe, BindingKind, BindingRecord, BindingTarget, Observed};
use crate::error::Result;
use crate::layout::{Class, Layout};
use crate::volume::{Volume, VolumeState};
use crate::workflow::{self, FileContents};
use crate::Barge;

/// Everything doctor found, and whether it is all ok.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// True when every volume is present with an agreeing layout and every
    /// class folder exists, and every binding is `ok`.
    pub ok: bool,
    pub volume_reports: Vec<VolumeReport>,
    pub binding_reports: Vec<BindingReport>,
    /// Areas whose recorded folder is missing while their roots sit elsewhere on
    /// the volume: a relocation that stopped between its rename and its record.
    pub misplaced_areas: Vec<MisplacedArea>,
}

impl DoctorReport {
    /// How many volumes, bindings and misplaced areas are not ok.
    pub fn problems(&self) -> usize {
        self.volume_reports
            .iter()
            .filter(|volume| !volume.ok())
            .count()
            + self
                .binding_reports
                .iter()
                .filter(|binding| !binding.state.is_ok())
                .count()
            + self.misplaced_areas.len()
    }
}

/// An area whose declared folder is missing while its roots were found at
/// `found`. Nothing repairs it; the report says where the roots are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MisplacedArea {
    pub area: String,
    pub recorded: PathBuf,
    pub found: PathBuf,
}

/// One registered volume.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeReport {
    pub name: String,
    pub mount: PathBuf,
    pub state: VolumeState,
    /// `None` when the volume is absent or another drive is at its mount, so
    /// there is no layout to check.
    pub layout: Option<LayoutReport>,
}

impl VolumeReport {
    pub fn ok(&self) -> bool {
        matches!(self.state, VolumeState::Present)
            && self.layout.as_ref().is_some_and(LayoutReport::ok)
    }
}

/// The layout as the volume shows it.
#[derive(Debug, Clone, Serialize)]
pub struct LayoutReport {
    /// Whether the volume record and the account copy agree.
    pub agrees: bool,
    pub classes: Vec<ClassFolder>,
    /// Top-level items at the volume's base that no class or area claims.
    pub unknown: Vec<PathBuf>,
}

impl LayoutReport {
    pub fn ok(&self) -> bool {
        self.agrees && self.classes.iter().all(|class| class.exists)
    }
}

/// One class folder on a volume.
#[derive(Debug, Clone, Serialize)]
pub struct ClassFolder {
    pub class: Class,
    pub path: PathBuf,
    pub exists: bool,
}

/// One recorded binding and what doctor sees now.
#[derive(Debug, Clone, Serialize)]
pub struct BindingReport {
    pub consumer: String,
    pub class: Class,
    pub kind: BindingKind,
    /// The file the binding owns: the link Barge made, or the scratch file in the
    /// repository.
    pub path: PathBuf,
    #[serde(flatten)]
    pub state: BindingState,
}

/// The state of one binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BindingState {
    Ok,
    /// The target or its value moved away from what Barge applied.
    Drifted {
        observed: Option<PathBuf>,
        expected: PathBuf,
        reason: String,
    },
    /// A workflow block is there but is not what the layout renders.
    Stale {
        reason: String,
    },
    /// Nothing is at the target.
    Missing,
    /// The class or its volume cannot be reached right now.
    Unavailable {
        reason: String,
    },
}

impl BindingState {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Where a volume is and where its classes sit.
struct Facts {
    state: VolumeState,
    base: PathBuf,
}

impl Barge {
    /// Report every volume, layout copy and binding. Writes nothing.
    pub fn doctor(&self) -> Result<DoctorReport> {
        let layout = self.layout()?;
        let mut facts = BTreeMap::new();
        let mut volumes = Vec::new();
        for volume in self.volumes()? {
            let state = volume.state()?;
            let report = match state {
                VolumeState::Present => Some(self.layout_report(&layout, &volume)?),
                _ => None,
            };
            facts.insert(
                volume.name().to_string(),
                Facts {
                    state: state.clone(),
                    base: volume.base(),
                },
            );
            volumes.push(VolumeReport {
                name: volume.name().to_string(),
                mount: volume.spec().mount.clone(),
                state,
                layout: report,
            });
        }
        let mut bindings = Vec::new();
        for record in self.bindings().list()? {
            let binding =
                binding_report(&layout, &facts, &record).unwrap_or_else(|error| BindingReport {
                    consumer: record.consumer.clone(),
                    class: record.class,
                    kind: record.kind,
                    path: record.owned_path(),
                    state: BindingState::Unavailable {
                        reason: error.message().to_string(),
                    },
                });
            bindings.push(binding);
        }
        let mut misplaced_areas = Vec::new();
        for area in self.areas()? {
            if let Some(found) = area.displaced_folder()? {
                misplaced_areas.push(MisplacedArea {
                    area: area.key().label(),
                    recorded: area.volume().base().join(area.record().effective_folder()),
                    found,
                });
            }
        }
        let ok = volumes.iter().all(VolumeReport::ok)
            && bindings.iter().all(|binding| binding.state.is_ok())
            && misplaced_areas.is_empty();
        Ok(DoctorReport {
            ok,
            volume_reports: volumes,
            binding_reports: bindings,
            misplaced_areas,
        })
    }

    fn layout_report(&self, layout: &Layout, volume: &Volume) -> Result<LayoutReport> {
        let agrees = !self.layout_disagrees(volume.name())?;
        let base = volume.base();
        let mut classes = Vec::new();
        for (class, placement) in &layout.classes {
            if placement.volume != volume.name() {
                continue;
            }
            let path = base.join(&placement.path);
            let exists = fs::metadata(&path)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false);
            classes.push(ClassFolder {
                class: *class,
                path,
                exists,
            });
        }
        let unknown = if base.is_dir() {
            self.survey(volume.name(), false)?
                .into_iter()
                .filter(|item| item.classification == crate::layout::SurveyClassification::Unknown)
                .map(|item| item.path)
                .collect()
        } else {
            Vec::new()
        };
        Ok(LayoutReport {
            agrees,
            classes,
            unknown,
        })
    }
}

fn binding_report(
    layout: &Layout,
    facts: &BTreeMap<String, Facts>,
    record: &BindingRecord,
) -> Result<BindingReport> {
    #[cfg(test)]
    if FAIL_BINDING.with(|flag| flag.replace(false)) {
        return Err(crate::Error::io(
            "cannot inspect",
            &record.owned_path(),
            &std::io::Error::other("injected binding error"),
        ));
    }
    let path = record.owned_path();
    let report = |state| BindingReport {
        consumer: record.consumer.clone(),
        class: record.class,
        kind: record.kind,
        path: path.clone(),
        state,
    };
    let Some(placement) = layout.classes.get(&record.class) else {
        return Ok(report(BindingState::Unavailable {
            reason: format!("class {} has no layout", record.class.name()),
        }));
    };
    let Some(volume) = facts.get(&placement.volume) else {
        return Ok(report(BindingState::Unavailable {
            reason: format!("volume {} is not registered", placement.volume),
        }));
    };
    if record.kind == BindingKind::Workflow {
        return Ok(report(workflow_state(layout, volume, record)?));
    }
    match &volume.state {
        VolumeState::Present => {}
        VolumeState::Absent => {
            return Ok(report(BindingState::Unavailable {
                reason: format!("volume {} is absent", placement.volume),
            }))
        }
        VolumeState::Mismatch { registered, found } => {
            return Ok(report(BindingState::Unavailable {
                reason: format!("volume at the mount is {found}, not the registered {registered}"),
            }))
        }
    }
    let class_folder = volume.base.join(&placement.path);
    let state = match observe(&path)? {
        Observed::Absent => BindingState::Missing,
        observed @ (Observed::Directory { .. } | Observed::File { .. }) => BindingState::Drifted {
            observed: Some(path.clone()),
            expected: record.applied.value.clone(),
            reason: format!("the target is {observed}, not a link"),
        },
        Observed::Link { destination } => {
            let resolved = fs::canonicalize(&path).ok();
            if resolved.as_deref() != Some(record.applied.value.as_path()) {
                BindingState::Drifted {
                    observed: resolved.or(Some(destination)),
                    expected: record.applied.value.clone(),
                    reason: "the link points somewhere else".to_string(),
                }
            } else if destination_replaced(record) {
                BindingState::Drifted {
                    observed: resolved,
                    expected: record.applied.value.clone(),
                    reason: "the destination was replaced".to_string(),
                }
            } else {
                match fs::canonicalize(&class_folder) {
                    Ok(folder) if !record.applied.value.starts_with(&folder) => {
                        BindingState::Drifted {
                            observed: Some(record.applied.value.clone()),
                            expected: folder,
                            reason: "the applied value is outside the class folder".to_string(),
                        }
                    }
                    _ => BindingState::Ok,
                }
            }
        }
    };
    Ok(report(state))
}

/// Whether the destination entry is no longer the one Barge linked. A record
/// written before identities were captured cannot tell, and is compared by path
/// alone, which is what doctor did before.
fn destination_replaced(record: &BindingRecord) -> bool {
    let Some(expected) = record.applied.identity else {
        return false;
    };
    match crate::bindings::entry_identity(&record.applied.value) {
        Ok(current) => current != expected,
        // The link resolved, so an entry is there; a race that removed it since
        // is reported by the next doctor run rather than guessed at here.
        Err(_) => false,
    }
}

/// What a recorded scratch file is now. Unlike a link, it renders while the
/// volume is away: the file names a location, not a mounted path.
fn workflow_state(layout: &Layout, volume: &Facts, record: &BindingRecord) -> Result<BindingState> {
    let BindingTarget::Workflow {
        repository,
        project,
        file,
    } = &record.target
    else {
        return Ok(BindingState::Drifted {
            observed: None,
            expected: record.applied.value.clone(),
            reason: "the record is not a workflow target".to_string(),
        });
    };
    // A record that names the committed file predates `barge init` writing the
    // local one. Doctor only says so; rerunning init replaces the record.
    if file == workflow::COMMITTED_FILE {
        return Ok(BindingState::Stale {
            reason: format!(
                "the binding records {}, which barge init no longer writes; rerun barge init",
                workflow::COMMITTED_FILE
            ),
        });
    }
    let rendered = match workflow::render(layout, &volume.base, project) {
        Ok(rendered) => rendered,
        Err(error) => {
            return Ok(BindingState::Unavailable {
                reason: error.message().to_string(),
            })
        }
    };
    let path = repository.join(file);
    let state = match workflow::read_file(&path)? {
        FileContents::Missing => BindingState::Missing,
        FileContents::NotText => BindingState::Stale {
            reason: format!("{file} is not valid UTF-8"),
        },
        FileContents::Text(content) if content == rendered.content => BindingState::Ok,
        FileContents::Text(_) => BindingState::Stale {
            reason: format!("the scratch file {file} is stale"),
        },
    };
    Ok(state)
}

#[cfg(test)]
thread_local! { static FAIL_BINDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn binding_error_does_not_abort_doctor() {
        let temp = crate::testing::TempDir::new("doctor-errors").unwrap();
        let mount = temp.folder("mount").unwrap();
        let state = temp.folder("state").unwrap();
        let os = Arc::new(crate::FakeOs::with_volume(
            &mount,
            "11111111-2222-3333-4444-555555555555",
            100,
        ));
        let b = Barge::open_with(state, os, Arc::new(crate::ManualClock::new(1))).unwrap();
        b.register_volume(crate::VolumeRegistration::new("disk", &mount).floor_bytes(0))
            .unwrap();
        b.set_layout(
            Class::Caches,
            crate::Placement {
                volume: "disk".into(),
                path: "caches".into(),
                absent: crate::Absent::Fail,
            },
        )
        .unwrap();
        let dest = mount.join("caches");
        std::fs::create_dir(&dest).unwrap();
        for name in ["one", "two"] {
            let link = temp.child(name);
            std::os::unix::fs::symlink(&dest, &link).unwrap();
            b.bindings()
                .adopt(&crate::Binding::link(name, Class::Caches, link))
                .unwrap();
        }
        FAIL_BINDING.with(|flag| flag.set(true));
        let report = b.doctor().unwrap();
        assert_eq!(report.binding_reports.len(), 2);
        assert!(
            matches!(&report.binding_reports[0].state, BindingState::Unavailable { reason } if reason.contains("injected binding error"))
        );
        assert!(matches!(report.binding_reports[1].state, BindingState::Ok));
    }
}
