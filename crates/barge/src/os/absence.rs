//! Read-only, bounded proof of a consumer path's presence or absence.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::{
    locate_volume, DirHandle, DirId, DirOps, EntryKind, FileSystem, MountRoot, VolumeFacts,
    VolumeIdentity, VolumeLocator, VolumeUuid,
};
use crate::error::{Error, ErrorKind, Result};

/// Descriptor-based volume facts needed for an absence proof.
pub trait AbsenceVolume: VolumeFacts + VolumeLocator + MountRoot {
    /// The recorded mount prefix if the path is under the platform's mount parent.
    fn volume_prefix(&self, path: &Path) -> Option<PathBuf>;
    /// Identify the volume through the held directory descriptor.
    fn identify_directory(&self, handle: &DirHandle) -> Result<VolumeIdentity>;
    /// Identify the current boot Data volume through its descriptor.
    fn boot_data(&self) -> Result<VolumeIdentity>;
}

/// The volume the consumer record requires for its nearest existing ancestor.
#[derive(Debug)]
pub enum AncestorExpected<'a> {
    /// The live boot Data volume.
    BootData,
    /// A registered volume; the recorded mount is the old path prefix.
    Registered {
        uuid: &'a VolumeUuid,
        recorded_mount: &'a Path,
    },
}

/// The evidence document; it must be established again before an effect.
#[derive(Debug, Serialize)]
pub struct AbsenceProof {
    schema: &'static str,
    path: PathBuf,
    verdict: &'static str,
    anchor: Anchor,
    #[serde(skip_serializing_if = "Option::is_none")]
    ancestor: Option<Ancestor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    leaf: Option<Leaf>,
    components_walked: usize,
    proved_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct Anchor {
    kind: &'static str,
    path: PathBuf,
}
#[derive(Debug, Serialize)]
struct Ancestor {
    path: PathBuf,
    inode: u64,
    device: u64,
    volume: VolumeEvidence,
}
#[derive(Debug, Serialize)]
struct VolumeEvidence {
    kind: &'static str,
    volume_uuid: VolumeUuid,
    file_system: String,
}
#[derive(Debug, Serialize)]
struct Leaf {
    name: String,
    result: &'static str,
}

impl AbsenceProof {
    /// The outcome: `absent`, `present`, or `unknown`.
    pub fn verdict(&self) -> &str {
        self.verdict
    }
    /// The reason an observation was uncertain, if any.
    pub fn reason(&self) -> Option<&str> {
        self.reason
    }
}

/// Prove a path from a held anchor. Only an ENOENT leaf on a verified volume is absent.
pub fn prove_absence(
    dirs: &dyn DirOps,
    volumes: &dyn AbsenceVolume,
    path: &Path,
    expected: AncestorExpected<'_>,
) -> Result<AbsenceProof> {
    #[cfg(not(test))]
    let proof = prove_absence_inner(dirs, volumes, path, expected)?;
    #[cfg(test)]
    let proof = match prove_absence_inner(dirs, volumes, path, expected) {
        Ok(proof) => proof,
        Err(error) => {
            crate::tape::read_refused("os::prove_absence", path, error.kind().as_str());
            return Err(error);
        }
    };
    let encoded = serde_json::to_vec(&proof).map_err(|_| {
        #[cfg(test)]
        crate::tape::read_refused("os::prove_absence", path, "encode_error");
        Error::new(
            ErrorKind::UnknownState,
            "absence evidence cannot be encoded",
        )
    })?;
    if encoded.len() > 4096 {
        #[cfg(test)]
        crate::tape::read_refused("os::prove_absence", path, "document_cap");
        return Err(Error::new(
            ErrorKind::UnknownState,
            "absence evidence exceeds 4 KiB",
        ));
    }
    #[cfg(test)]
    crate::tape::verdict(
        "os::prove_absence",
        crate::tape::Identity::file(path, None),
        proof.verdict,
    );
    Ok(proof)
}

fn prove_absence_inner(
    dirs: &dyn DirOps,
    volumes: &dyn AbsenceVolume,
    path: &Path,
    expected: AncestorExpected<'_>,
) -> Result<AbsenceProof> {
    let raw = path.to_str().ok_or_else(|| usage("path is not UTF-8"))?;
    if !raw.starts_with('/')
        || raw
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(usage(
            "path must be absolute and contain only named components",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let prefix = volumes.volume_prefix(path);
    let mut proof = AbsenceProof {
        schema: "barge.absence/1",
        path: path.to_path_buf(),
        verdict: "unknown",
        anchor: Anchor {
            kind: "root",
            path: PathBuf::from("/"),
        },
        ancestor: None,
        leaf: None,
        components_walked: 0,
        proved_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        reason: None,
    };
    #[cfg(test)]
    crate::tape::now_ms_site(proof.proved_ms, "os::prove_absence_inner");
    let registered = match expected {
        AncestorExpected::BootData => None,
        AncestorExpected::Registered {
            uuid,
            recorded_mount,
        } => {
            if prefix.as_deref().is_some_and(|p| p != recorded_mount) {
                return Ok(unknown(proof, "volume_unverified"));
            }
            match locate_volume(volumes, volumes, volumes, uuid) {
                Ok(volume) => Some(volume),
                Err(_) => return Ok(unknown(proof, "volume_unverified")),
            }
        }
    };
    let relative = if let Some(prefix) = prefix {
        let Some(volume) = &registered else {
            return Ok(unknown(proof, "volume_unverified"));
        };
        proof.anchor = Anchor {
            kind: "registered",
            path: volume.path().to_path_buf(),
        };
        path.strip_prefix(prefix)
            .map_err(|_| usage("invalid mount prefix"))?
    } else {
        path.strip_prefix("/")
            .map_err(|_| usage("invalid absolute path"))?
    };
    let components: Vec<&str> = relative
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or(""))
        .collect();
    if components.is_empty() || components.iter().any(|name| name.is_empty()) {
        return Err(usage("path has no component below anchor"));
    }
    if components.len() > 64 {
        return Ok(unknown(proof, "depth_bound"));
    }
    let mut current_path = proof.anchor.path.clone();
    let mut handle = match dirs.open_path(&current_path) {
        Ok(h) => h,
        Err(_) => return Ok(unknown(proof, "volume_unverified")),
    };
    if let Some(volume) = &registered {
        if proof.anchor.kind == "registered" && handle.identity().device != volume.identity().device
        {
            return Ok(unknown(proof, "volume_unverified"));
        }
    }
    for (index, name) in components.iter().enumerate() {
        if Instant::now() >= deadline {
            return Ok(unknown(proof, "deadline"));
        }
        let checked = match dirs.child_facts(&handle, name) {
            Ok(value) => value,
            Err(_) => return Ok(unknown(proof, "not_readable")),
        };
        let Some(checked) = checked else {
            if index + 1 != components.len() {
                return Ok(unknown(proof, "ancestor_missing"));
            }
            let actual = match volumes.identify_directory(&handle) {
                Ok(value) => value,
                Err(_) => return Ok(unknown(proof, "volume_unverified")),
            };
            let reference = match &registered {
                Some(volume) => volume.identity().clone(),
                None => match volumes.boot_data() {
                    Ok(value) => value,
                    Err(_) => return Ok(unknown(proof, "volume_unverified")),
                },
            };
            let valid_fs = matches!(&actual.filesystem, FileSystem::Apfs)
                || matches!(&actual.filesystem, FileSystem::Other(name) if name.eq_ignore_ascii_case("hfs"));
            if !valid_fs
                || actual.device != handle.identity().device
                || actual.device != reference.device
                || actual.volume_uuid.is_none()
                || actual.volume_uuid != reference.volume_uuid
                || actual.filesystem != reference.filesystem
                || actual.mount_point != reference.mount_point
                || Instant::now() >= deadline
            {
                return Ok(unknown(proof, "volume_unverified"));
            }
            if let Some(volume) = &registered {
                if volume.reverify(volumes).is_err() {
                    return Ok(unknown(proof, "volume_unverified"));
                }
            } else if !reference.boot_volume {
                return Ok(unknown(proof, "volume_unverified"));
            }
            if Instant::now() >= deadline {
                return Ok(unknown(proof, "deadline"));
            }
            let Some(uuid) = actual.volume_uuid else {
                return Ok(unknown(proof, "volume_unverified"));
            };
            proof.ancestor = Some(Ancestor {
                path: current_path,
                inode: handle.identity().inode,
                device: handle.identity().device.get(),
                volume: VolumeEvidence {
                    kind: if registered.is_some() {
                        "registered"
                    } else {
                        "boot_data"
                    },
                    volume_uuid: uuid,
                    file_system: actual.filesystem.as_str().to_owned(),
                },
            });
            proof.leaf = Some(Leaf {
                name: (*name).to_owned(),
                result: "enoent",
            });
            proof.components_walked = index;
            proof.verdict = "absent";
            return Ok(proof);
        };
        if checked.kind != EntryKind::Directory {
            return Ok(unknown(
                proof,
                if checked.kind == EntryKind::Symlink {
                    "symlink_component"
                } else {
                    "not_a_directory"
                },
            ));
        }
        let opened = match dirs.open_child_dir(&handle, name) {
            Ok(value) => value,
            Err(_) => return Ok(unknown(proof, "not_readable")),
        };
        if opened.identity()
            != (DirId {
                device: checked.device,
                inode: checked.inode,
            })
        {
            return Ok(unknown(proof, "replaced"));
        }
        handle = opened;
        current_path.push(name);
        proof.components_walked = index + 1;
    }
    if Instant::now() >= deadline {
        return Ok(unknown(proof, "deadline"));
    }
    proof.verdict = "present";
    Ok(proof)
}

fn unknown(mut proof: AbsenceProof, reason: &'static str) -> AbsenceProof {
    proof.reason = Some(reason);
    proof
}
fn usage(message: &str) -> Error {
    Error::new(ErrorKind::Usage, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::os::{DeviceId, MacOs, MountPoint};
    use crate::testkit::TempDir;
    use std::cell::RefCell;

    struct Facts {
        device: DeviceId,
        removable: bool,
        located: RefCell<Vec<VolumeUuid>>,
    }
    impl Facts {
        fn identity(&self) -> VolumeIdentity {
            VolumeIdentity {
                mount_point: PathBuf::from("/System/Volumes/Data"),
                filesystem: FileSystem::Apfs,
                device: self.device,
                volume_uuid: Some(
                    VolumeUuid::parse("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D").unwrap(),
                ),
                boot_volume: true,
            }
        }
    }
    impl VolumeFacts for Facts {
        fn identify(&self, _: &Path) -> Result<VolumeIdentity> {
            Ok(self.identity())
        }
    }
    impl VolumeLocator for Facts {
        fn mounts_for_uuid(&self, uuid: &VolumeUuid) -> Result<Vec<MountPoint>> {
            self.located.borrow_mut().push(uuid.clone());
            Ok(vec![])
        }
    }
    impl MountRoot for Facts {
        fn mount_root(&self, _: &Path) -> Result<MountPoint> {
            Err(usage("not mounted"))
        }
    }
    impl AbsenceVolume for Facts {
        fn volume_prefix(&self, path: &Path) -> Option<PathBuf> {
            let prefix = Path::new("/Volumes/Fixture");
            (self.removable && path.starts_with(prefix)).then(|| prefix.to_path_buf())
        }
        fn identify_directory(&self, _: &DirHandle) -> Result<VolumeIdentity> {
            Ok(self.identity())
        }
        fn boot_data(&self) -> Result<VolumeIdentity> {
            Ok(self.identity())
        }
    }
    #[test]
    fn invalid_path_refusal_is_recorded_without_a_platform_read() {
        use crate::tape::{begin_window, finish, EngineOutcome, WindowConfig};
        let facts = Facts { device: DeviceId::new(1), removable: false, located: RefCell::new(Vec::new()) };
        begin_window(WindowConfig {
            case: "invalid".to_owned(),
            manifest_sha256: "test".to_owned(),
            entrypoint_symbol: "os::prove_absence",
            source_binding: "test".to_owned(),
        }).unwrap();
        let error = prove_absence(&MacOs::new(), &facts, Path::new("relative"), AncestorExpected::BootData).unwrap_err();
        let transcript = finish(EngineOutcome {
            status: "refused", exit_code: error.kind().exit_code(),
            refusal_kind: Some(error.kind().as_str().to_owned()),
            refusal_message: Some(error.message().to_owned()),
            refusal_detail: Vec::new(), engine_truncated: false,
        }, None).unwrap();
        let document: serde_json::Value = serde_json::from_str(&transcript).unwrap();
        assert_eq!(document["reads"][0]["site"], "os::prove_absence");
        assert_eq!(document["reads"][0]["observation_proven"], false);
        assert!(document["reads"][0]["outcome"].get("refused").is_some());
    }

    #[test]
    fn missing_leaf_requires_verified_ancestor_and_missing_parent_is_unknown() {
        let fixture = TempDir::new("absence");
        let dirs = MacOs::new();
        let parent = dirs.open_path(fixture.path()).unwrap();
        let facts = Facts {
            device: parent.identity().device,
            removable: false,
            located: RefCell::new(Vec::new()),
        };
        let path = fixture.path().join("gone");
        let proof = prove_absence(&dirs, &facts, &path, AncestorExpected::BootData).unwrap();
        assert_eq!(proof.verdict(), "absent");
        let json = serde_json::to_value(&proof).unwrap();
        assert_eq!(json["schema"], "barge.absence/1");
        assert_eq!(json["ancestor"]["volume"]["kind"], "boot_data");
        assert_eq!(
            prove_absence(
                &dirs,
                &facts,
                &path.join("child"),
                AncestorExpected::BootData
            )
            .unwrap()
            .reason(),
            Some("ancestor_missing")
        );
        let link = fixture.path().join("link");
        std::os::unix::fs::symlink(fixture.path(), &link).unwrap();
        assert_eq!(
            prove_absence(
                &dirs,
                &facts,
                &link.join("child"),
                AncestorExpected::BootData
            )
            .unwrap()
            .reason(),
            Some("symlink_component")
        );
        let deep = PathBuf::from(format!("/{}", vec!["x"; 65].join("/")));
        assert_eq!(
            prove_absence(&dirs, &facts, &deep, AncestorExpected::BootData)
                .unwrap()
                .reason(),
            Some("depth_bound")
        );
        assert_eq!(
            prove_absence(&dirs, &facts, fixture.path(), AncestorExpected::BootData)
                .unwrap()
                .verdict(),
            "present"
        );
        let foreign = Facts {
            device: DeviceId::new(parent.identity().device.get() + 1),
            removable: false,
            located: RefCell::new(Vec::new()),
        };
        assert_eq!(
            prove_absence(&dirs, &foreign, &path, AncestorExpected::BootData)
                .unwrap()
                .reason(),
            Some("volume_unverified")
        );
    }

    #[test]
    fn an_unmounted_registered_volume_is_unknown_after_the_uuid_lookup() {
        let dirs = MacOs::new();
        let facts = Facts {
            device: DeviceId::new(1),
            removable: true,
            located: RefCell::new(Vec::new()),
        };
        let uuid = facts.identity().volume_uuid.unwrap();
        let path = Path::new("/Volumes/Fixture/project/gone");
        let mismatch = prove_absence(
            &dirs,
            &facts,
            path,
            AncestorExpected::Registered {
                uuid: &uuid,
                recorded_mount: Path::new("/Volumes/Other"),
            },
        )
        .unwrap();
        assert_eq!(mismatch.reason(), Some("volume_unverified"));
        assert!(
            facts.located.borrow().is_empty(),
            "a mismatched prefix refuses before lookup"
        );
        let unmounted = prove_absence(
            &dirs,
            &facts,
            path,
            AncestorExpected::Registered {
                uuid: &uuid,
                recorded_mount: Path::new("/Volumes/Fixture"),
            },
        )
        .unwrap();
        assert_eq!(unmounted.verdict(), "unknown");
        assert_eq!(unmounted.reason(), Some("volume_unverified"));
        assert_eq!(
            *facts.located.borrow(),
            vec![uuid],
            "the matching UUID was looked up and found unmounted"
        );
    }
}
