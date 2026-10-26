#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The S6 native proof's harness preconditions.
//!
//! The elevated S6 native proof runs one fixed script
//! (`scripts/native/s6-cache-placement-proof.sh`) as its whole command, and that
//! script executes Oxide's qualified shared supervisor
//! (`~/Dev/projects/oxide/scripts/bounded-test.py`) at pinned bytes. The
//! supervisor is a Python file that Oxide keeps mode 0644 and runs with
//! `python3`, so the script's gate must be readability plus the digest, never
//! the execute bit: an owner-activated run once refused a 0644 supervisor
//! ("is not executable") and never started the guard.
//!
//! These tests prove the harness contract without running any part of the
//! proof: they read the script's source for the pin, the readable-regular-file
//! gate and the interpreter invocation, and they drive the script's outer half
//! against fixture supervisors whose bytes are not the pinned ones, so a 0644
//! file must get past the mode gate and be refused for its digest.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// The pinned qualified supervisor digest, exactly as the plan and the script
/// state it.
const PINNED_SUPERVISOR_SHA256: &str =
    "bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924";

/// The fixed script under test.
fn proof_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/native/s6-cache-placement-proof.sh")
}

/// A temporary directory that removes itself, created under this process's own
/// temporary directory and never at a fixed path.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = base.join(format!(
            "barge-native-harness-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("the fixture directory is created");
        Self { path }
    }

    fn write(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path.join(name);
        std::fs::write(&path, bytes).expect("the fixture file is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("the fixture mode is set");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Run the fixed script's outer half with a fixture supervisor.
///
/// `OXIDE_PROOF_EVIDENCE` is removed so the script takes its outer path; it must
/// never reach the interpreter with a fixture whose digest is not the pinned
/// one.
fn run_outer_half(supervisor: &Path, binary: &Path) -> Output {
    Command::new("/bin/bash")
        .arg(proof_script())
        .arg("diagnostic")
        .arg(binary)
        .env("BARGE_S6_SUPERVISOR", supervisor)
        .env_remove("OXIDE_PROOF_EVIDENCE")
        .env_remove("OXIDE_TASK_SCRATCH_ROOT")
        .output()
        .expect("the proof script starts")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_script_pins_the_supervisor_bytes_and_runs_them_with_python() {
    let source = std::fs::read_to_string(proof_script()).expect("the proof script is readable");

    assert!(
        source.contains(&format!(
            "readonly SUPERVISOR_SHA256=\"{PINNED_SUPERVISOR_SHA256}\""
        )),
        "the exact pinned supervisor digest is a constant"
    );
    assert!(
        !source.contains("! -x \"$SUPERVISOR\""),
        "the supervisor's execute bit must never be a precondition"
    );
    assert!(
        source.contains("[[ ! -f \"$SUPERVISOR\" || ! -r \"$SUPERVISOR\" ]]"),
        "the supervisor is gated as a readable regular file"
    );
    assert!(
        source.contains("exec /usr/bin/python3 \"$SUPERVISOR\""),
        "the pinned bytes are executed by /usr/bin/python3"
    );
    assert!(
        !source.contains("chmod"),
        "the proof never changes the supervisor's mode"
    );
    assert!(
        source.contains("readonly LIMIT_SECONDS=300"),
        "the task's 300 s wall clock is the fixed bound"
    );
    assert!(
        source.contains("readonly SCRATCH_BUDGET_MIB=512"),
        "the task's 512 MiB monitored scratch is the fixed bound"
    );
    assert!(
        source.contains("--limit \"$LIMIT_SECONDS\"")
            && source.contains("--scratch-budget-mib \"$SCRATCH_BUDGET_MIB\""),
        "both bounds are passed to the pinned supervisor"
    );
}

#[test]
fn a_0644_supervisor_is_accepted_by_the_mode_gate_and_refused_by_the_pin() {
    let fixture = Fixture::new("supervisor-0644");
    let supervisor = fixture.write(
        "bounded-test.py",
        b"#!/usr/bin/env python3\nprint('a different supervisor')\n",
        0o644,
    );
    let binary = fixture.write("barge", b"#!/bin/sh\nexit 0\n", 0o755);

    let output = run_outer_half(&supervisor, &binary);
    assert_eq!(
        output.status.code(),
        Some(125),
        "a different supervisor is refused before anything runs"
    );
    let message = stderr(&output);
    assert!(
        message.contains("not the pinned") && message.contains(PINNED_SUPERVISOR_SHA256),
        "the refusal names the pinned digest: {message}"
    );
    assert!(
        !message.contains("not executable") && !message.contains("readable regular file"),
        "a 0644 supervisor is not refused for its mode: {message}"
    );
}

#[test]
fn an_executable_supervisor_with_other_bytes_is_refused_for_the_same_reason() {
    let fixture = Fixture::new("supervisor-0755");
    let supervisor = fixture.write(
        "bounded-test.py",
        b"#!/usr/bin/env python3\nprint('also a different supervisor')\n",
        0o755,
    );
    let binary = fixture.write("barge", b"#!/bin/sh\nexit 0\n", 0o755);

    let output = run_outer_half(&supervisor, &binary);
    assert_eq!(output.status.code(), Some(125));
    let message = stderr(&output);
    assert!(
        message.contains("not the pinned") && message.contains(PINNED_SUPERVISOR_SHA256),
        "the digest is the gate, not the mode: {message}"
    );
}
