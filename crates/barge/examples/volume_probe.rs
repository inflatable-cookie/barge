//! Read-only OS-layer diagnostic: print the volume facts for one directory and
//! verify the volume against them.
//!
//! It opens the directory, reads the facts through the `os` traits, prints them,
//! and then runs the production verification policy against the facts it just
//! read: the volume's own UUID must match and its kernel mount point must be
//! proved to be a mount root. The directory may be any directory on the volume,
//! not only its mount root. Nothing is created, written, renamed or deleted and
//! no lock is taken.
//!
//! A directory on a volume Barge may not manage — the boot volume, or one that
//! fails the mount-parent rule — reports the facts and then the refusal that
//! names why, so it is also safe to point at those while diagnosing. It is
//! evidence for the OS layer, not a test: run it through
//! `effigy os:volumes:probe` with `BARGE_OS_PROBE_PATH` naming the directory.

use std::path::PathBuf;
use std::process::ExitCode;

use barge::error::{Error, ErrorKind};
use barge::os::{self, ExpectedVolume, VolumeFacts};

fn main() -> ExitCode {
    let Some(path) = std::env::var_os("BARGE_OS_PROBE_PATH").map(PathBuf::from) else {
        eprintln!("BARGE_OS_PROBE_PATH must name the directory to read");
        return ExitCode::from(ErrorKind::Usage.exit_code());
    };

    let platform = os::platform();
    let identity = match platform.identify(&path) {
        Ok(identity) => identity,
        Err(error) => return refuse(&error, "verified=false"),
    };
    println!("path={}", path.display());
    println!("mount_point={}", identity.mount_point.display());
    println!("filesystem={}", identity.filesystem);
    println!("device={}", identity.device);
    let Some(volume_uuid) = &identity.volume_uuid else {
        println!("volume_uuid=unreadable");
        return refuse(
            &Error::new(
                ErrorKind::UnknownState,
                "the volume UUID is unreadable, so the volume cannot be verified",
            ),
            "verified=false",
        );
    };
    println!("volume_uuid={volume_uuid}");
    println!("boot_volume={}", identity.boot_volume);

    // The production policy, against the identity that was just read: this is
    // what proves the mount-point composition on a real volume.
    let expected = ExpectedVolume {
        volume_uuid,
        mount_hint: Some(&identity.mount_point),
    };
    match os::verify_volume(&platform, &platform, &path, &expected) {
        Ok(_) => {
            println!("verified=true");
            ExitCode::SUCCESS
        }
        Err(error) => refuse(&error, "verified=false"),
    }
}

/// Report a refusal on stderr, mark the outcome on stdout and exit with the
/// kind's own code.
fn refuse(error: &Error, outcome: &str) -> ExitCode {
    eprintln!("barge: {}: {error}", error.kind());
    println!("{outcome}");
    ExitCode::from(error.kind().exit_code())
}
