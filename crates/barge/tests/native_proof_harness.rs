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
    // A failing CLI call must name itself: every case goes through the wrapper,
    // which echoes the invocation, the document and its stderr into the case log.
    assert!(
        source.contains("barge_cli() {") && !source.contains("\"$BARGE_BIN\" cache"),
        "every case's CLI call goes through the evidence-capturing wrapper"
    );
    assert!(
        source.contains("stdout: ") && source.contains("stderr: "),
        "the wrapper echoes the CLI document and its stderr"
    );
    // Every case log is retained beside the receipt, and the receipt names it.
    assert!(
        source.contains("readonly CASE_LOGS=\"$EVIDENCE/cases\""),
        "case logs are retained in the supervisor's evidence directory"
    );
    assert!(
        source.contains("\"log\": log if os.path.exists(log) else None"),
        "the receipt names each retained case log"
    );
    // The hand-made layout case creates its tool directory before linking.
    assert!(
        source.contains("mkdir -p \"$HOME/.cargo\"\n  ln -s \"$target\""),
        "the hand-made case creates $HOME/.cargo before linking"
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

// ---- the per-case harness preflight ----------------------------------------
//
// Every case that runs is preceded by two required descriptor observations made
// through the same macOS primitive the product's observer uses:
// `proc_pidinfo(PROC_PIDLISTFDS)` for the listing and
// `proc_pidinfo(PROC_PIDTBSDINFO)` for the same-uid "kinfo" fact. One sentinel
// child is started in the shared supervisor's guarded session, listed while it
// is alive, then terminated and reaped inside the fixed bound; and one
// pre-existing process of this account is listed after the same fact. Either
// capability failing is an infrastructure abort, never a case result.
//
// The probe bodies are delimited in the script so these tests can run them
// directly, below the elevated tier, against a fixture supervisor and fixture
// scratch. No image, supervisor, selector or native case runs here.

/// The delimiters around the probe block.
const PREFLIGHT_BEGIN: &str = "# >>> s6-preflight";
const PREFLIGHT_END: &str = "# <<< s6-preflight";

/// The proof script's source.
fn proof_source() -> String {
    std::fs::read_to_string(proof_script()).expect("the proof script is readable")
}

/// The probe block, exactly as shipped.
fn preflight_block() -> String {
    let source = proof_source();
    let start = source
        .find(PREFLIGHT_BEGIN)
        .expect("the preflight block has a begin marker")
        + PREFLIGHT_BEGIN.len();
    let rest = &source[start..];
    let end = rest
        .find(PREFLIGHT_END)
        .expect("the preflight block has an end marker");
    let block = rest[..end].to_string();
    for function in [
        "descriptor_observation() {",
        "preflight_sentinel() {",
        "preflight_existing_descriptors() {",
    ] {
        assert!(block.contains(function), "the block defines {function}");
    }
    block
}

/// The SHA-256 of one fixture file, computed the same way the script does.
fn digest_of(path: &Path) -> String {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .expect("shasum runs");
    assert!(output.status.success(), "shasum succeeds");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("a digest is printed")
        .to_string()
}

/// One preflight fixture: a private root, its scratch, a fixture supervisor and
/// the shipped probe block, run in one bash process.
struct ProbeRun {
    fixture: Fixture,
    output: Output,
}

impl ProbeRun {
    fn root(&self) -> &Path {
        &self.fixture.path
    }

    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    /// The observations the probes appended, one row of tab-separated fields.
    fn observations(&self) -> Vec<Vec<String>> {
        let path = self.root().join("observations.tsv");
        let text = std::fs::read_to_string(path).expect("the observation file is readable");
        text.lines()
            .map(|line| line.split('\t').map(str::to_string).collect())
            .collect()
    }

    fn observation(&self, probe: &str) -> Option<Vec<String>> {
        self.observations()
            .into_iter()
            .find(|row| row.get(1).is_some_and(|name| name == probe))
    }
}

/// Run the shipped probe block in a fresh fixture.
///
/// `pinned` is the digest the fixture supervisor is held to; `None` uses the
/// fixture's own digest, so the supervisor probe passes.
fn run_probe_block(label: &str, pinned: Option<&str>, body: &str) -> ProbeRun {
    let fixture = Fixture::new(label);
    let root = fixture.path.clone();
    std::fs::create_dir_all(root.join("scratch")).expect("the fixture scratch is created");
    let supervisor = fixture.write(
        "supervisor.py",
        b"#!/usr/bin/env python3\nprint('fixture supervisor')\n",
        0o644,
    );
    let digest = digest_of(&supervisor);
    let pinned = pinned.unwrap_or(&digest).to_string();
    let block = preflight_block();
    fixture.write("preflight-block.sh", block.as_bytes(), 0o644);
    let script = format!(
        "set -uo pipefail\n\
         ROOT=\"{root}\"\n\
         PREFLIGHT_SESSION=\"$(/bin/ps -o sess= -p $$ | tr -d ' ')\"\n\
         PREFLIGHT_SENTINEL_BOUND_SECONDS=2\n\
         PREFLIGHT_RUNS=0\n\
         PREFLIGHT_FAILURE=\"\"\n\
         PREFLIGHT_LAST_SENTINEL=\"\"\n\
         PREFLIGHT_LAST_EXISTING=\"\"\n\
         PREFLIGHT_TSV=\"$ROOT/observations.tsv\"\n\
         : >\"$PREFLIGHT_TSV\"\n\
         SUPERVISOR=\"$ROOT/supervisor.py\"\n\
         SUPERVISOR_SHA256=\"{pinned}\"\n\
         OXIDE_TASK_SCRATCH_ROOT=\"$ROOT/scratch\"\n\
         TMPDIR=\"$ROOT/scratch\"\n\
         source \"$ROOT/preflight-block.sh\"\n\
         printf 'HARNESS_SESSION=%s\\n' \"$PREFLIGHT_SESSION\"\n\
         {body}\n",
        root = root.display(),
        pinned = pinned,
        body = body
    );
    let harness = fixture.write("harness.sh", script.as_bytes(), 0o644);
    let output = Command::new("/bin/bash")
        .arg(&harness)
        .output()
        .expect("the probe harness runs");
    ProbeRun { fixture, output }
}

/// One `name=value` line's value.
fn named_line(text: &str, prefix: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("{prefix} is printed: {text}"))
        .trim()
        .to_string()
}

#[test]
fn the_probe_block_uses_the_kernel_descriptor_primitive_and_reaps_its_sentinel() {
    let block = preflight_block();
    // The same primitive the product's observer uses, for both halves.
    assert!(
        block.contains("PROC_PIDLISTFDS = 1"),
        "the listing flavor is the kernel's"
    );
    assert!(
        block.contains("PROC_PIDTBSDINFO = 3"),
        "the same-uid fact is the kernel's"
    );
    assert!(
        block.contains("libc.proc_pidinfo(pid, PROC_PIDTBSDINFO"),
        "the kinfo fact is read before any listing"
    );
    assert!(
        block.contains("libc.proc_pidinfo(\n    pid, PROC_PIDLISTFDS"),
        "the listing is the real proc_pidinfo call"
    );
    assert!(
        block
            .find("reported_uid, problem = kinfo(pid)")
            .expect("the kinfo fact is read")
            < block
                .find("PROC_PIDLISTFDS, 0, buf")
                .expect("the listing follows the fact"),
        "the same-uid fact precedes the descriptor listing"
    );
    // The sentinel is this shell's own child, terminated and reaped inside a
    // bound, so nothing escapes into the session after the preflight.
    assert!(
        block.contains("pid=$!"),
        "the sentinel is a child of this shell"
    );
    assert!(
        block.contains("kill -TERM \"$pid\""),
        "the sentinel is terminated"
    );
    assert!(
        block.contains("kill -KILL \"$pid\""),
        "a stuck sentinel is still bounded"
    );
    assert!(block.contains("wait \"$pid\""), "the sentinel is reaped");
    assert!(
        proof_source().contains("readonly PREFLIGHT_SENTINEL_BOUND_SECONDS=2"),
        "the sentinel bound is a fixed constant"
    );
    // No escape hatch: no new session, no detached child.
    for escape in ["setsid", "nohup", "disown", "set -m"] {
        assert!(
            !proof_source().contains(escape),
            "the proof never uses {escape} to leave the guarded session"
        );
    }
}

#[test]
fn a_successful_preflight_proves_both_descriptor_listings() {
    let run = run_probe_block(
        "preflight-ok",
        None,
        "if preflight \"case-one\" \"$ROOT/case-one/preflight\"; then\n\
         \x20 printf 'PREFLIGHT=ok runs=%s\\n' \"$PREFLIGHT_RUNS\"\n\
         else\n\
         \x20 printf 'PREFLIGHT=failed %s\\n' \"$PREFLIGHT_FAILURE\"\n\
         fi\n\
         printf 'IDENTITY='; cat \"$ROOT/case-one/preflight/sentinel.identity\"",
    );
    assert_eq!(
        run.output.status.code(),
        Some(0),
        "stderr: {}",
        run.stderr()
    );
    let stdout = run.stdout();
    assert!(
        stdout.contains("PREFLIGHT=ok runs=1"),
        "{stdout} {}",
        run.stderr()
    );
    let session = named_line(&stdout, "HARNESS_SESSION=");
    let identity = named_line(&stdout, "IDENTITY=");
    let identity_session = identity
        .split_once("session=")
        .expect("the sentinel records its session")
        .1
        .trim();
    assert_eq!(
        identity_session, session,
        "the sentinel ran in this session"
    );

    let sentinel = run
        .observation("sentinel-descriptors")
        .expect("the sentinel's descriptor listing is recorded");
    let fields: Vec<&str> = sentinel[2].split_whitespace().collect();
    assert_eq!(fields.first().copied(), Some("sentinel"), "{sentinel:?}");
    assert!(
        fields[2].parse::<u32>().expect("a descriptor count") >= 1,
        "the live sentinel's descriptors were listed: {sentinel:?}"
    );
    assert_eq!(fields[4], "9", "the identity descriptor is observed");
    assert_eq!(fields[5], "1", "the identity descriptor is a live vnode");

    let existing = run
        .observation("existing-descriptors")
        .expect("the pre-existing process listing is recorded");
    let fields: Vec<&str> = existing[2].split_whitespace().collect();
    assert_eq!(fields.first().copied(), Some("existing"), "{existing:?}");
    assert!(
        fields[2].parse::<u32>().expect("a descriptor count") >= 1,
        "a pre-existing same-uid process was listed: {existing:?}"
    );

    assert!(
        run.observation("supervisor").is_some(),
        "the supplemental supervisor identity check is recorded too"
    );

    let pid = identity
        .strip_prefix("pid=")
        .expect("the identity names its pid")
        .split_whitespace()
        .next()
        .expect("the pid is a field");
    let gone = Command::new("/bin/kill")
        .arg("-0")
        .arg(pid)
        .output()
        .expect("kill -0 runs");
    assert!(
        !gone.status.success(),
        "the sentinel was reaped, not left in the session"
    );
}

#[test]
fn a_failed_supervisor_probe_aborts_before_the_sentinel() {
    let run = run_probe_block(
        "preflight-pin-fail",
        Some("0000000000000000000000000000000000000000000000000000000000000000"),
        "if preflight \"case-x\" \"$ROOT/case-x/preflight\"; then\n\
         \x20 printf 'PREFLIGHT=unexpected\\n'\n\
         else\n\
         \x20 printf 'PREFLIGHT=failed %s\\n' \"$PREFLIGHT_FAILURE\"\n\
         fi",
    );
    let stdout = run.stdout();
    assert!(
        stdout.contains("PREFLIGHT=failed supervisor probe failed before case-x"),
        "{stdout} {}",
        run.stderr()
    );
    assert!(
        run.observations().is_empty(),
        "no probe observation is recorded on an abort"
    );
    assert!(
        !run.root()
            .join("case-x/preflight/sentinel.identity")
            .exists(),
        "the sentinel never starts when the supervisor probe fails"
    );
    assert!(
        run.stderr().contains("not the pinned"),
        "the refusal names the pin: {}",
        run.stderr()
    );
}

#[test]
fn a_failed_sentinel_descriptor_probe_aborts_the_case() {
    let run = run_probe_block(
        "preflight-sentinel-fail",
        None,
        "printf 'x' >\"$ROOT/blocker\"\n\
         if preflight \"case-y\" \"$ROOT/blocker/probe\"; then\n\
         \x20 printf 'PREFLIGHT=unexpected\\n'\n\
         else\n\
         \x20 printf 'PREFLIGHT=failed %s\\n' \"$PREFLIGHT_FAILURE\"\n\
         fi",
    );
    let stdout = run.stdout();
    assert!(
        stdout.contains("PREFLIGHT=failed sentinel descriptor probe failed before case-y"),
        "{stdout} {}",
        run.stderr()
    );
    assert!(
        run.observations().is_empty(),
        "an aborted preflight records no observation"
    );
    assert!(
        run.stderr()
            .contains("cannot create the sentinel probe directory"),
        "the sentinel failure names itself: {}",
        run.stderr()
    );
}

#[test]
fn an_unavailable_existing_process_listing_aborts_the_case() {
    let run = run_probe_block(
        "preflight-existing-fail",
        None,
        "if preflight_existing_descriptors 999999; then\n\
         \x20 printf 'EXISTING=unexpected\\n'\n\
         else\n\
         \x20 printf 'EXISTING=failed\\n'\n\
         fi",
    );
    assert!(run.stdout().contains("EXISTING=failed"), "{}", run.stderr());
    assert!(
        run.stderr().contains("kinfo fact of pid 999999")
            || run
                .stderr()
                .contains("no pre-existing same-uid descriptor listing"),
        "the refusal names the fact it could not read: {}",
        run.stderr()
    );
}

#[test]
fn the_preflight_refuses_a_changed_session() {
    let run = run_probe_block(
        "preflight-session-fail",
        None,
        "if preflight_supervisor \"$SUPERVISOR\" \"$SUPERVISOR_SHA256\" 999999; then\n\
         \x20 printf 'SESSION=unexpected\\n'\n\
         else\n\
         \x20 printf 'SESSION=failed\\n'\n\
         fi",
    );
    assert!(run.stdout().contains("SESSION=failed"), "{}", run.stderr());
    assert!(
        run.stderr().contains("is not the guarded session"),
        "the session check names the guard: {}",
        run.stderr()
    );
}

#[test]
fn every_case_is_preflighted_before_its_body_and_aborts_the_run() {
    let source = proof_source();
    let run_case = &source[source.find("run_case() {").expect("run_case exists")
        ..source
            .find("# ---- run --")
            .expect("the run section exists")];
    let preflight_at = run_case
        .find("preflight \"$id\" \"$dir/preflight\"")
        .expect("the case is preflighted");
    let body_at = run_case.find("if \"$body\"").expect("the case body runs");
    assert!(
        preflight_at < body_at,
        "the preflight precedes every case body"
    );
    assert!(
        run_case
            .contains("record \"$id\" \"skipped\" \"infrastructure abort: $PREFLIGHT_FAILURE\"")
            && run_case.contains("STOPPED=\"$id\""),
        "a failed probe stops the run instead of being recorded as a case result"
    );
    let preflight = &source[source.find("preflight() {").expect("preflight exists")
        ..source.find(PREFLIGHT_END).expect("the block ends")];
    assert!(
        preflight.contains("preflight_sentinel")
            && preflight.contains("preflight_existing_descriptors"),
        "both required descriptor probes run before a case"
    );
    assert!(
        source.contains("refuse \"preflight infrastructure abort: $PREFLIGHT_FAILURE"),
        "the run refuses with an infrastructure status, not a case failure"
    );
    assert!(
        source.contains("\"aborted\": preflight_failure is not None")
            && source.contains("and preflight_failure is None")
            && source.contains("and int(os.environ[\"S6_PREFLIGHT_RUNS\"]) == len(cases)"),
        "the receipt records the abort and can never be qualified by it"
    );
}
