//! Read-only OS-layer diagnostic: print the volume facts for one directory.
//!
//! It opens the directory, reads the facts through the `os` traits and prints
//! them. It creates, writes, renames and deletes nothing and takes no lock, so
//! it is safe to point at any directory this user can open. It is evidence for
//! the OS layer, not a test: run it through `effigy os:volumes:probe` with
//! `BARGE_OS_PROBE_PATH` naming the directory.

use std::path::PathBuf;
use std::process::ExitCode;

use barge::error::ErrorKind;
use barge::os::{self, VolumeFacts};

fn main() -> ExitCode {
    let Some(path) = std::env::var_os("BARGE_OS_PROBE_PATH").map(PathBuf::from) else {
        eprintln!("BARGE_OS_PROBE_PATH must name the directory to read");
        return ExitCode::from(ErrorKind::Usage.exit_code());
    };

    let platform = os::platform();
    match platform.identify(&path) {
        Ok(identity) => {
            println!("path={}", path.display());
            println!("mount_point={}", identity.mount_point.display());
            println!("filesystem={}", identity.filesystem);
            println!("device={}", identity.device);
            match &identity.volume_uuid {
                Some(volume_uuid) => println!("volume_uuid={volume_uuid}"),
                None => println!("volume_uuid=unreadable"),
            }
            println!("boot_volume={}", identity.boot_volume);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("barge: {}: {error}", error.kind());
            ExitCode::from(error.kind().exit_code())
        }
    }
}
