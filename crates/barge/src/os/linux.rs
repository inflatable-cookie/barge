//! Linux filesystem identity from mountinfo and the kernel device links.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use crate::error::{Error, Result};

use super::canonical_uuid;

#[cfg(target_os = "linux")]
pub(super) fn volume_uuid(mount: &Path) -> Result<Option<String>> {
    let Ok(canonical) = std::fs::canonicalize(mount) else {
        return Ok(None);
    };
    let path = Path::new("/proc/self/mountinfo");
    let info =
        std::fs::read(path).map_err(|e| Error::io("cannot read mount information at", path, &e))?;
    let Some((source, subvol_id)) = mount_source(&info, &canonical) else {
        return Ok(None);
    };
    if !source.starts_with("/dev") {
        return Ok(None);
    }
    Ok(uuid_for_source(&source, Path::new("/dev/disk/by-uuid"))
        .map(|uuid| volume_identity(uuid, subvol_id)))
}

// Mountinfo escapes whitespace and backslashes as octal bytes. Paths need not
// be UTF-8. Reject malformed escapes instead of guessing a path.
fn unescape(field: &[u8]) -> Option<PathBuf> {
    let mut bytes = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' {
            let digits = field.get(i + 1..i + 4)?;
            if !digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return None;
            }
            let value = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            bytes.push(u8::try_from(value).ok()?);
            i += 4;
        } else {
            bytes.push(field[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn volume_identity(uuid: String, subvol_id: Option<u64>) -> String {
    match subvol_id {
        Some(id) if id != 5 => format!("{uuid}:subvol={id}"),
        _ => uuid,
    }
}

fn mount_source(info: &[u8], mount: &Path) -> Option<(PathBuf, Option<u64>)> {
    let mut found = None;
    for line in info.split(|b| *b == b'\n') {
        let fields: Vec<_> = line
            .split(|b| *b == b' ')
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() < 10 || unescape(fields[4]).as_deref() != Some(mount) {
            continue;
        }
        // Stacked entries are ambiguous: never guess which one is visible.
        if found.is_some() {
            return None;
        }
        let separator = fields.iter().position(|field| *field == b"-")?;
        if separator < 6 || separator + 3 >= fields.len() {
            return None;
        }
        let root = unescape(fields[3])?;
        let subvol_id = if fields[separator + 1] == b"btrfs" {
            let mut subvol = None;
            let mut id = None;
            for option in fields[separator + 3].split(|byte| *byte == b',') {
                if let Some(value) = option.strip_prefix(b"subvol=") {
                    if subvol.replace(unescape(value)?).is_some() {
                        return None;
                    }
                } else if let Some(value) = option.strip_prefix(b"subvolid=") {
                    let parsed = std::str::from_utf8(value).ok()?.parse::<u64>().ok()?;
                    if parsed == 0 || id.replace(parsed).is_some() {
                        return None;
                    }
                }
            }
            let id = id?;
            if subvol.as_deref() != Some(root.as_path()) || (id == 5 && root != Path::new("/")) {
                return None;
            }
            Some(id)
        } else {
            if root != Path::new("/") {
                return None;
            }
            None
        };
        found = Some((unescape(fields[separator + 2])?, subvol_id));
    }
    found
}

fn uuid_for_source(source: &Path, links: &Path) -> Option<String> {
    let device = std::fs::canonicalize(source).ok()?;
    let entries = std::fs::read_dir(links).ok()?;
    let mut found = None;
    for entry in entries {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_symlink()
            || std::fs::canonicalize(entry.path()).ok().as_ref() != Some(&device)
        {
            continue;
        }
        let name = entry.file_name();
        let Some(uuid) = name.to_str().and_then(|name| canonical_uuid(name).ok()) else {
            continue;
        };
        if found.as_ref().is_some_and(|previous| previous != &uuid) {
            return None;
        }
        found = Some(uuid);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn parses_mountinfo_and_rejects_subfolders_and_non_mounts() {
        let info = b"31 20 8:1 / /mnt/my\\040disk rw shared:1 - ext4 /dev/sda1 rw\n32 20 8:1 /folder /bind rw - ext4 /dev/sda1 rw\n33 20 0:2 / /tmp rw - tmpfs tmpfs rw\n";
        assert_eq!(
            mount_source(info, Path::new("/mnt/my disk")),
            Some((PathBuf::from("/dev/sda1"), None))
        );
        assert_eq!(mount_source(info, Path::new("/mnt/my disk/sub")), None);
        assert_eq!(mount_source(info, Path::new("/bind")), None);
        assert_eq!(
            mount_source(info, Path::new("/tmp")),
            Some((PathBuf::from("tmpfs"), None))
        );
        assert_eq!(
            mount_source(
                b"1 0 8:1 / /x rw - ext4 /dev/a rw\n2 0 8:2 / /x rw - ext4 /dev/b rw\n",
                Path::new("/x")
            ),
            None
        );
    }

    #[test]
    fn btrfs_subvolume_identity_and_bind_mounts() {
        let info = b"74 236 0:36 /home /var/home rw - btrfs /dev/a rw,subvolid=257,subvol=/home\n75 236 0:36 / /top rw - btrfs /dev/a rw,subvolid=5,subvol=/\n76 236 0:36 /home/user/x /bind rw - btrfs /dev/a rw,subvolid=257,subvol=/home\n77 236 0:36 /other\\040space /mnt/other\\040space rw - btrfs /dev/a rw,subvolid=258,subvol=/other\\040space\n";
        let identity = |mount| {
            mount_source(info, Path::new(mount))
                .map(|(_, id)| volume_identity("UUID".to_owned(), id))
        };
        assert_eq!(identity("/var/home").as_deref(), Some("UUID:subvol=257"));
        assert_eq!(identity("/top").as_deref(), Some("UUID"));
        assert_eq!(identity("/bind"), None);
        assert_eq!(
            identity("/mnt/other space").as_deref(),
            Some("UUID:subvol=258")
        );
        assert_ne!(identity("/var/home"), identity("/mnt/other space"));
        assert_eq!(
            mount_source(
                b"1 0 0:1 /home /x rw - btrfs /dev/a rw,subvol=/home\n",
                Path::new("/x")
            ),
            None
        );
    }

    #[test]
    fn matches_uuid_links_and_refuses_unknown_sources() {
        let dir = crate::testing::TempDir::new("linux-uuid").unwrap();
        let root = dir.path();
        let device = root.join("device");
        std::fs::write(&device, b"").unwrap();
        let links = root.join("links");
        std::fs::create_dir(&links).unwrap();
        symlink(
            "../device",
            links.join("abcdef01-2345-6789-abcd-ef0123456789"),
        )
        .unwrap();
        assert_eq!(
            uuid_for_source(&device, &links).as_deref(),
            Some("ABCDEF01-2345-6789-ABCD-EF0123456789")
        );
        assert_eq!(uuid_for_source(Path::new("tmpfs"), &links), None);
    }
}
