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

use std::collections::{BTreeMap, BTreeSet};
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
         PREFLIGHT_LAST_CENSUS=\"\"\n\
         PREFLIGHT_TSV=\"$ROOT/observations.tsv\"\n\
         : >\"$PREFLIGHT_TSV\"\n\
         SUPERVISOR=\"$ROOT/supervisor.py\"\n\
         SUPERVISOR_SHA256=\"{pinned}\"\n\
         OXIDE_TASK_SCRATCH_ROOT=\"$ROOT/scratch\"\n\
         TMPDIR=\"$ROOT/scratch\"\n\
         source \"$ROOT/preflight-block.sh\"\n\
         preflight_existing_descriptors() {{\n\
         \x20 PREFLIGHT_LAST_CENSUS='{{\"status\":\"passed\",\"processes\":2,\"descriptors\":5}}'\n\
         }}\n\
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

    // Full-census policy is exercised separately against injected kernel facts;
    // the preflight wrapper must record its result alongside the live sentinel.
    let existing = run
        .observation("full-census")
        .expect("the census is recorded");
    let census: serde_json::Value = serde_json::from_str(&existing[2]).expect("census JSON");
    assert_eq!(census["status"], "passed");
    assert_eq!(census["processes"], 2);

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
        "preflight_existing_descriptors() {\n\
         \x20 PREFLIGHT_LAST_CENSUS='{\"status\":\"infrastructure\",\"pid\":1299,\"command\":\"fixture\",\"problem\":\"EPERM\"}'\n\
         \x20 return 1\n\
         }\n\
         if preflight case-z \"$ROOT/case-z/preflight\"; then\n\
         \x20 printf 'EXISTING=unexpected\\n'\n\
         else\n\
         \x20 printf 'EXISTING=failed\\n'\n\
         fi",
    );
    assert!(run.stdout().contains("EXISTING=failed"), "{}", run.stderr());
    let census = run
        .observation("full-census")
        .expect("failed census retained");
    assert!(census[2].contains("1299") && census[2].contains("EPERM"));
    assert!(census[2].contains("fixture"));
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

/// Extract only the shipped oracle or case under test; never start the proof.
fn source_between(begin: &str, end: &str) -> String {
    let source = proof_source();
    let start = source.find(begin).expect("the block begins") + begin.len();
    let rest = &source[start..];
    rest[..rest.find(end).expect("the block ends")].to_string()
}

#[test]
fn the_full_census_classifies_every_same_uid_process_and_preserves_its_bounds() {
    let fixture = Fixture::new("full-census");
    let census = source_between("# >>> s6-descriptor-census", "# <<< s6-descriptor-census");
    let body = r#"
import errno, os
uid = os.geteuid()
def row(pid, owner=uid):
    return {"pid": pid, "uid": owner, "command": "fixture-" + str(pid)}

# Inject at the kernel boundary, never invoke sysctl or proc_pidinfo.
calls = []
def listing(pid):
    calls.append(pid)
    return 2
descriptor_count = listing
process_table = lambda: [row(100001), row(100002), row(100003, uid + 1), row(os.getpid())]
result = census()
assert result["status"] == "passed", result
assert calls == [100001, 100002], calls
assert result["processes"] == 2 and result["descriptors"] == 4, result
assert result["foreign"] == 1 and result["self_excluded"] == 1, result

def denied(pid):
    raise OSError(errno.EPERM, "listing denied")
descriptor_count = denied
process_table = lambda: [row(1299)]
result = census()
assert result["status"] == "infrastructure" and result["pid"] == 1299, result
assert result["command"] == "fixture-1299" and "listing denied" in result["problem"], result

def exited(pid):
    if pid == 100001:
        raise OSError(errno.ESRCH, "exited")
    return 0
descriptor_count = exited
process_table = lambda: [row(100001), row(100002)]
result = census()
assert result["status"] == "passed" and result["exited"] == 1, result
assert result["processes"] == 2 and result["descriptors"] == 0, result

# Every fixed bound must refuse instead of publishing a partial pass.
for count, descriptors, problem in [
    (8193, 0, "process list"), (4097, 0, "processes"),
    (1, 4096, "descriptors per process"), (17, 4095, "descriptors"),
]:
    process_table = lambda: [row(100000 + i) for i in range(count)]
    descriptor_count = lambda pid: descriptors
    result = census()
    assert result["status"] == "infrastructure" and problem in result["problem"], result
    assert "pid" in result and "command" in result, result

process_table = lambda: [row(100001)]
clock = iter([0, 10])
time.monotonic = lambda: next(clock)
result = census()
assert result["status"] == "infrastructure" and result["problem"] == "time bound", result
assert result["pid"] == 100001 and result["command"] == "fixture-100001", result
"#;
    // Execute the definitions as a fixture module: the native main never runs.
    let script = fixture.write("census.py", format!("{census}\n{body}").as_bytes(), 0o644);
    let output = Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import runpy,sys; runpy.run_path(sys.argv[1], run_name='fixture')",
        ])
        .arg(script)
        .output()
        .expect("the pure census fixture runs");
    assert!(output.status.success(), "{}", stderr(&output));

    // Drift from the product's process set or limits is a harness defect.
    let source = proof_source();
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    let custody = std::fs::read_to_string(package.join("src/custody.rs")).expect("custody source");
    let platform = std::fs::read_to_string(package.join("src/os/macos.rs")).expect("OS source");
    for (python, rust) in [
        ("SCAN_PROCESSES = 4096", "SCAN_PROCESSES: usize = 4096"),
        (
            "SCAN_DESCRIPTORS = 65_536",
            "SCAN_DESCRIPTORS: usize = 65_536",
        ),
        ("SCAN_TIMEOUT = 10", "Duration::from_secs(10)"),
    ] {
        assert!(source.contains(python) && custody.contains(rust));
    }
    for (python, rust) in [
        ("SCAN_PID_CAP = 8192", "SCAN_PID_CAP: usize = 8192"),
        ("KINFO_PROC_SIZE = 648", "KINFO_PROC_SIZE: usize = 648"),
        (
            "SCAN_DESCRIPTORS_PER_PROCESS = 4096",
            "SCAN_DESCRIPTORS_PER_PROCESS: usize = 4096",
        ),
    ] {
        assert!(source.contains(python) && platform.contains(rust));
    }
    assert!(source.contains("(1, 14, 0)  # CTL_KERN/KERN_PROC/KERN_PROC_ALL"));
    assert!(source.contains("libc.proc_pidinfo(pid, 1, 0, buf, len(buf))"));
}

#[test]
fn the_adopted_tree_oracle_tolerates_only_the_root_marker_metadata_changes() {
    let fixture = Fixture::new("adopted-tree-oracle");
    let oracle = source_between(
        "# >>> s6-adopted-tree-digest",
        "# <<< s6-adopted-tree-digest",
    );
    let body = r#"
mkdir -p cache/nested
printf 'registry data' >cache/payload
printf 'nested marker is payload' >cache/nested/.barge-root.json
ln -s payload cache/payload-link
before="$(digest_tree_excluding cache .barge-root.json)"
printf marker >cache/.barge-root.json
touch -mt 200001010000 cache
[[ "$(digest_tree_excluding cache .barge-root.json)" == "$before" ]]
rm cache/.barge-root.json
touch -mt 200101010000 cache
[[ "$(digest_tree_excluding cache .barge-root.json)" == "$before" ]]

# Same-size changed bytes with the original mtime still have to be noticed.
/usr/bin/python3 - <<'PY'
import os
p = 'cache/payload'
s = os.stat(p)
with open(p, 'wb') as f:
    f.write(b'registry DATA')
os.utime(p, ns=(s.st_atime_ns, s.st_mtime_ns))
PY
[[ "$(digest_tree_excluding cache .barge-root.json)" != "$before" ]]
before="$(digest_tree_excluding cache .barge-root.json)"
mv cache/payload cache/renamed
[[ "$(digest_tree_excluding cache .barge-root.json)" != "$before" ]]
mv cache/renamed cache/payload
[[ "$(digest_tree_excluding cache .barge-root.json)" == "$before" ]]

rm cache/payload-link
ln -s other-payload cache/payload-link
[[ "$(digest_tree_excluding cache .barge-root.json)" != "$before" ]]
before="$(digest_tree_excluding cache .barge-root.json)"
printf changed >cache/nested/.barge-root.json
[[ "$(digest_tree_excluding cache .barge-root.json)" != "$before" ]]
if digest_tree_excluding missing .barge-root.json; then
  echo 'an unreadable tree was accepted' >&2
  exit 1
fi
"#;
    let script = fixture.write(
        "oracle.sh",
        format!("set -euo pipefail\n{oracle}\n{body}").as_bytes(),
        0o644,
    );
    let output = Command::new("/bin/bash")
        .arg(script)
        .current_dir(&fixture.path)
        .output()
        .expect("the pure oracle fixture runs");
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn the_hand_made_case_uses_one_oracle_for_adopt_and_undo_and_keeps_the_link() {
    let oracle = source_between(
        "# >>> s6-adopted-tree-digest",
        "# <<< s6-adopted-tree-digest",
    );
    let case = source_between(
        "\ncase_hand_made_in_place() {",
        "\ncase_plan_effect_free() {",
    );
    // Model only marker publication/removal, without a CLI, supervisor or image.
    // JSON conformance belongs to the product tests; here the shipped case must
    // tolerate the expected root changes and refuse payload/link changes.
    let body = r#"
export HOME="$PWD/home"
CASE_CACHES="$PWD/caches"
CASE_OUT="$PWD/out"
UUID=fixture
mkdir -p "$HOME" "$CASE_OUT"
assert_json() { return 0; }
barge_cli() {
  local target="$CASE_CACHES/cargo/registry"
  if [[ "$3" == adopt ]]; then
    printf marker >"$target/.barge-root.json"
    touch -mt 200001010000 "$target"
    case "$S6_FIXTURE_CHANGE" in
      payload) printf changed >"$target/payload" ;;
      moved) mv "$target/payload" "$target/renamed" ;;
      link)
        rm "$HOME/.cargo/registry"
        ln -s "$CASE_CACHES/other" "$HOME/.cargo/registry"
        ;;
      swapped-link)
        mv "$HOME/.cargo/registry" "$HOME/.cargo/old-link"
        ln -s "$target" "$HOME/.cargo/registry"
        ;;
    esac
  else
    rm "$target/.barge-root.json"
    touch -mt 200101010000 "$target"
    if [[ "$S6_FIXTURE_CHANGE" == undo-payload ]]; then
      printf changed >"$target/payload"
    fi
  fi
}
case_hand_made_in_place
"#;
    for (change, expected) in [
        ("none", 0),
        ("payload", 1),
        ("moved", 1),
        ("link", 1),
        ("swapped-link", 1),
        ("undo-payload", 1),
    ] {
        let fixture = Fixture::new(&format!("hand-made-{change}"));
        let script = fixture.write(
            "case.sh",
            format!("set -uo pipefail\n{oracle}\ncase_hand_made_in_place() {{{case}\n{body}")
                .as_bytes(),
            0o644,
        );
        let output = Command::new("/bin/bash")
            .arg(script)
            .current_dir(&fixture.path)
            .env("S6_FIXTURE_CHANGE", change)
            .output()
            .expect("the pure hand-made case fixture runs");
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{change}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            stderr(&output)
        );
    }
}

#[test]
fn the_shipped_verdict_executes_without_stray_prose_and_preserves_failures() {
    let source = proof_source();
    let start = source
        .find("# The run's own verdict:")
        .expect("the verdict begins");
    let verdict = &source[start..];
    for (preflight, fault, expected) in [("", "", 0), ("", "case", 1), ("probe", "", 125)] {
        let fixture = Fixture::new("verdict");
        let script = fixture.write(
            "verdict.sh",
            format!(
                "set -uo pipefail\nrefuse() {{ exit 125; }}\nMODE=diagnostic\nRECEIPT=fixture\n{verdict}"
            )
            .as_bytes(),
            0o644,
        );
        let output = Command::new("/bin/bash")
            .arg(script)
            .env("PREFLIGHT_FAILURE", preflight)
            .env("FIRST_FAULT", fault)
            .output()
            .expect("the pure verdict fixture runs");
        assert_eq!(output.status.code(), Some(expected), "{}", stderr(&output));
        assert!(
            !stderr(&output).contains("command not found"),
            "only verdict commands execute: {}",
            stderr(&output)
        );
    }
}

// ---- the nine-premise trace ------------------------------------------------
//
// The design's nine-premise HIGH checklist must be traced through the case
// bodies, not asserted in prose. The script ships one row per
// (premise, case, concrete assertion) in `write_premise_trace`, every `run_case`
// declares the premise ids its body demonstrates, and the receipt folds the two
// against the recorded case results and requires all nine for a counted pass.
// These tests run the shipped trace writer and the shipped receipt body, and
// prove the map is complete, that the plan, the trace and the case list agree,
// and that the receipt can only qualify a run whose mapped cases all passed.

/// The delimiters around the shipped premise trace.
const TRACE_BEGIN: &str = "# >>> s6-premise-trace";
const TRACE_END: &str = "# <<< s6-premise-trace";

/// The delimiters around the shipped receipt body.
const RECEIPT_BEGIN: &str = "# >>> s6-receipt";
const RECEIPT_END: &str = "# <<< s6-receipt";

/// The shipped premise trace, exactly as written before the cases run.
fn premise_trace_block() -> String {
    let source = proof_source();
    let start = source
        .find(TRACE_BEGIN)
        .expect("the premise trace has a begin marker")
        + TRACE_BEGIN.len();
    let rest = &source[start..];
    let end = rest
        .find(TRACE_END)
        .expect("the premise trace has an end marker");
    let block = rest[..end].to_string();
    assert!(
        block.contains("write_premise_trace() {"),
        "the block writes the trace"
    );
    block
}

/// The shipped receipt body, the Python between `<<'PY'` and its terminator.
fn receipt_body() -> String {
    let source = proof_source();
    let start = source
        .find(RECEIPT_BEGIN)
        .expect("the receipt has a begin marker")
        + RECEIPT_BEGIN.len();
    let end = source[start..]
        .find(RECEIPT_END)
        .expect("the receipt has an end marker")
        + start;
    let block = &source[start..end];
    let mut body = Vec::new();
    let mut started = false;
    let mut closed = false;
    for line in block.lines() {
        if !started {
            if line.contains("<<'PY'") {
                started = true;
            }
            continue;
        }
        if line == "PY" {
            closed = true;
            break;
        }
        body.push(line);
    }
    assert!(started, "the receipt body starts at its heredoc");
    assert!(closed, "the receipt heredoc closes");
    assert!(
        body.iter().any(|line| line.contains("import json")),
        "the receipt body is the Python program"
    );
    body.join("\n")
}

/// One traced row: a premise, the case body that asserts it, and the assertion.
struct PremiseRow {
    premise: u32,
    case: String,
    asserts: String,
}

/// Run the shipped trace writer against a fixture run root.
fn shipped_premise_rows() -> (Fixture, Vec<PremiseRow>) {
    let fixture = Fixture::new("premise-trace");
    let root = &fixture.path;
    fixture.write("trace-block.sh", premise_trace_block().as_bytes(), 0o644);
    let script = format!(
        "set -uo pipefail\n\
         ROOT=\"{root}\"\n\
         PREMISE_TRACE_TSV=\"$ROOT/premise-trace.tsv\"\n\
         : >\"$PREMISE_TRACE_TSV\"\n\
         source \"$ROOT/trace-block.sh\"\n\
         write_premise_trace\n\
         cat \"$PREMISE_TRACE_TSV\"\n",
        root = root.display()
    );
    let harness = fixture.write("trace.sh", script.as_bytes(), 0o644);
    let output = Command::new("/bin/bash")
        .arg(&harness)
        .output()
        .expect("the trace writer runs");
    assert!(
        output.status.success(),
        "the trace writer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let rows = text
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(
                fields.len(),
                3,
                "one row is premise, case and assertion: {line}"
            );
            PremiseRow {
                premise: fields[0].parse::<u32>().expect("a premise id is a number"),
                case: fields[1].to_string(),
                asserts: fields[2].to_string(),
            }
        })
        .collect();
    (fixture, rows)
}

/// Every case body the runner declares, with the premise ids it demonstrates.
fn declared_case_premises() -> BTreeMap<String, BTreeSet<u32>> {
    let source = proof_source();
    let mut declared = BTreeMap::new();
    for line in source.lines() {
        let Some(rest) = line.strip_prefix("run_case ") else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(
            fields.len(),
            4,
            "run_case takes id, depends, body and premises: {line}"
        );
        let id = fields[0].to_string();
        let premises: BTreeSet<u32> = fields[3]
            .trim_matches('"')
            .split(',')
            .filter(|part| !part.is_empty())
            .map(|part| part.parse::<u32>().expect("a premise id is a number"))
            .collect();
        assert!(!premises.is_empty(), "{id} declares at least one premise");
        declared.insert(id, premises);
    }
    declared
}

#[test]
fn the_premise_trace_maps_every_premise_to_a_case_body() {
    let (_fixture, rows) = shipped_premise_rows();
    assert!(!rows.is_empty(), "the trace writes rows");

    let declared = declared_case_premises();
    let mut premise_rows: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    let mut case_rows: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for row in &rows {
        assert!(
            !row.asserts.trim().is_empty(),
            "premise {} for {} names its concrete assertion",
            row.premise,
            row.case
        );
        assert!(
            declared.contains_key(&row.case),
            "a trace row cites a declared case body: {}",
            row.case
        );
        premise_rows
            .entry(row.premise)
            .or_default()
            .insert(row.case.clone());
        case_rows
            .entry(row.case.clone())
            .or_default()
            .insert(row.premise);
    }

    let premises: BTreeSet<u32> = (1..=9).collect();
    assert_eq!(
        premise_rows.keys().copied().collect::<BTreeSet<u32>>(),
        premises,
        "every premise 1-9 is mapped to a case body"
    );
    assert_eq!(
        case_rows.keys().cloned().collect::<BTreeSet<String>>(),
        declared.keys().cloned().collect::<BTreeSet<String>>(),
        "every case body is mapped at least once"
    );
    for (case, expected) in &declared {
        assert_eq!(
            case_rows.get(case),
            Some(expected),
            "{case}: the declared premises equal the citing rows"
        );
    }

    // The receipt turns the shipped rows into evidence and needs all nine.
    let source = proof_source();
    assert!(
        source.contains("S6_PREMISE_TRACE=\"$PREMISE_TRACE_TSV\""),
        "the receipt reads the shipped trace file"
    );
    assert!(
        source.contains("\"asserts\": asserts"),
        "each observation carries its concrete assertion"
    );
    assert!(
        source.contains("\"premise_trace\": premise_trace"),
        "the receipt carries the folded trace"
    );
    assert!(
        source.contains("and premise_trace[\"complete\"]"),
        "a counted receipt needs a complete premise trace"
    );
    assert!(
        source.contains("record \"$id\" \"pass\" \"$(tr '\\n' ';' <\"$log\")\" \"$premises\""),
        "each case row records the premise ids it was declared for"
    );
    let trace_call = source
        .find("\nwrite_premise_trace\n")
        .expect("the trace is written at startup");
    let first_case = source
        .find("\nrun_case absent-path ")
        .expect("the cases run");
    assert!(
        trace_call < first_case,
        "the trace is written before the first case runs"
    );

    // The plan carries the same map, one row per premise and one per case.
    let docs = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/knowledge/contracts/s6-native-proof.md"),
    )
    .expect("the proof plan is readable");
    assert!(docs.contains("## The premise trace in the receipt"));
    for premise in 1..=9 {
        assert!(
            docs.contains(&format!("\n| {premise} | ")),
            "the plan has a premise row for {premise}"
        );
    }
    for case in declared.keys() {
        assert!(
            docs.contains(&format!("| `{case}` |")),
            "the plan has a case row for {case}"
        );
    }
}

/// Run the shipped receipt body against fabricated evidence and return the
/// receipt document it wrote.
fn fabricated_receipt(
    label: &str,
    statuses: &BTreeMap<String, &str>,
    census_passed: bool,
) -> (Fixture, serde_json::Value) {
    let (trace_fixture, rows) = shipped_premise_rows();
    let fixture = Fixture::new(label);
    let root = &fixture.path;
    let declared = declared_case_premises();
    std::fs::create_dir_all(root.join("cases")).expect("the case log directory is created");

    let mut cases = String::new();
    for (case, premises) in &declared {
        let ids = premises
            .iter()
            .map(u32::to_string)
            .collect::<Vec<String>>()
            .join(",");
        let status = statuses.get(case).copied().unwrap_or("pass");
        cases.push_str(&format!("{case}\t{status}\tthe {case} case log\t{ids}\n"));
    }
    std::fs::write(root.join("cases.tsv"), cases).expect("the fabricated cases are written");

    let mut trace = String::new();
    for row in &rows {
        trace.push_str(&format!("{}\t{}\t{}\n", row.premise, row.case, row.asserts));
    }
    std::fs::write(root.join("premise-trace.tsv"), trace).expect("the fabricated trace is written");

    let mut preflight = String::new();
    for case in declared.keys() {
        preflight.push_str(&format!("{case}\tsupervisor\tsession=1 pinned=fixture\n"));
        preflight.push_str(&format!(
            "{case}\tsentinel-descriptors\tsentinel 501 4 2 9 1\n"
        ));
        let status = if census_passed {
            "passed"
        } else {
            "infrastructure"
        };
        preflight.push_str(&format!(
            "{case}\tfull-census\t{{\"status\":\"{status}\",\"processes\":2,\"descriptors\":5}}\n"
        ));
    }
    std::fs::write(root.join("preflight.tsv"), preflight)
        .expect("the fabricated preflight observations are written");

    let receipt_path = root.join("receipt.json");
    let body = receipt_body();
    let program = fixture.write("receipt.py", body.as_bytes(), 0o644);
    let output = Command::new("/usr/bin/python3")
        .arg(&program)
        .env("S6_MODE", "counted")
        .env("S6_EXIT", "0")
        .env("S6_RECEIPT", &receipt_path)
        .env("S6_EVIDENCE", root)
        .env("S6_CASES", root.join("cases.tsv"))
        .env("S6_PREMISE_TRACE", root.join("premise-trace.tsv"))
        .env("S6_PREFLIGHT_TSV", root.join("preflight.tsv"))
        .env("S6_PREFLIGHT_RUNS", declared.len().to_string())
        .env("S6_PREFLIGHT_FAILURE", "")
        .env("S6_PREFLIGHT_SESSION", "1")
        .env("S6_PREFLIGHT_BOUND", "2")
        .env("S6_CASE_LOGS", root.join("cases"))
        .env("S6_HEAD", "fixture-head")
        .env("S6_REPO", root)
        .env("S6_BINARY", "/bin/true")
        .env("S6_SUPERVISOR", "/bin/true")
        .env("S6_SUPERVISOR_SHA", "fixture-supervisor")
        .env("S6_VOLUME", "BargeProofFixture")
        .env("S6_MOUNT", "/Volumes/BargeProofFixture")
        .env("S6_BACKING", root.join("fixture.sparseimage"))
        .env("S6_SCRATCH", root.join("scratch"))
        .env("S6_TMPDIR", root.join("scratch"))
        .env("S6_HOME", root.join("home"))
        .env("S6_CARGO_HOME", root.join("cargo-home"))
        .env("S6_STATE", root.join("state"))
        .env("S6_REAL_STATE", root.join("real-state"))
        .env("S6_REAL_BEFORE", "same")
        .env("S6_REAL_AFTER", "same")
        .env("S6_STARTED", "1")
        .env("S6_IMAGE_MIB", "128")
        .env("S6_LIMIT", "300")
        .env("S6_SCRATCH_MIB", "512")
        .output()
        .expect("the receipt body runs");
    assert!(
        output.status.success(),
        "the receipt body failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(&receipt_path).expect("the receipt is written");
    let document = serde_json::from_str(&text).expect("the receipt is one JSON document");
    drop(trace_fixture);
    (fixture, document)
}

#[test]
fn the_receipt_folds_the_premise_trace_against_case_results() {
    let mut statuses = BTreeMap::new();
    let (_fixture, document) = fabricated_receipt("premise-receipt-pass", &statuses, true);
    assert_eq!(
        document["qualified"], true,
        "a clean fixture receipt qualifies"
    );
    assert_eq!(document["premise_trace"]["premises"], 9);
    assert_eq!(document["premise_trace"]["covered"], 9);
    assert_eq!(document["premise_trace"]["complete"].as_bool(), Some(true));
    let censuses = document["preflight"]["censuses"]
        .as_array()
        .expect("censuses retained");
    assert_eq!(censuses.len(), 9);
    assert!(censuses
        .iter()
        .all(|row| row["status"] == "passed" && row["processes"] == 2));
    let premises = document["premises"]
        .as_array()
        .expect("the receipt lists the premises");
    assert_eq!(premises.len(), 9);
    for premise in premises {
        assert_eq!(premise["covered"].as_bool(), Some(true), "{premise}");
        let observations = premise["observations"]
            .as_array()
            .expect("each premise lists its observations");
        assert!(!observations.is_empty(), "{premise}");
        for observation in observations {
            assert_eq!(observation["status"], "pass", "{observation}");
            assert!(
                observation["asserts"]
                    .as_str()
                    .is_some_and(|assertion| !assertion.is_empty()),
                "{observation}"
            );
            assert!(
                observation["evidence"]
                    .as_str()
                    .is_some_and(|evidence| evidence.contains("case log")),
                "the case's own evidence line is folded in: {observation}"
            );
        }
    }
    let case_rows = document["cases"].as_array().expect("the cases are listed");
    for case in case_rows {
        assert!(
            case["premises"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty()),
            "each case row carries the premises it was declared for: {case}"
        );
    }

    let (_fixture, document) = fabricated_receipt("census-receipt-fail", &statuses, false);
    assert_eq!(
        document["qualified"], false,
        "failed censuses cannot qualify passing cases"
    );
    statuses.insert("ordinary-detach".to_string(), "fail");
    let (_fixture, document) = fabricated_receipt("premise-receipt-fail", &statuses, true);
    assert_eq!(
        document["qualified"], false,
        "a failed case never qualifies"
    );
    assert_eq!(document["premise_trace"]["complete"].as_bool(), Some(false));
    let missing = document["premise_trace"]["missing"]
        .as_array()
        .expect("the receipt names the missing premises");
    let missing: Vec<u64> = missing
        .iter()
        .map(|value| value.as_u64().expect("a premise id"))
        .collect();
    assert_eq!(
        missing,
        vec![2, 5, 9],
        "only this case's premises are missing"
    );
    let premises = document["premises"]
        .as_array()
        .expect("the receipt lists the premises");
    let premise_five = premises
        .iter()
        .find(|premise| premise["id"] == 5)
        .expect("premise 5 is listed");
    assert_eq!(premise_five["covered"].as_bool(), Some(false));
    assert!(
        premise_five["missing"]
            .as_array()
            .is_some_and(|cases| cases.iter().any(|case| case == "ordinary-detach")),
        "the premise names the case that did not pass: {premise_five}"
    );
}
