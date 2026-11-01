#!/usr/bin/env python3
"""Owner-activated stage 3 native proof under the qualified Oxide supervisor."""
import argparse
import hashlib
import importlib.util
import json
import os
import pwd
import subprocess
import sys
import tempfile
import time
import traceback
import uuid

sys.dont_write_bytecode = True
from space_cases import Cases
from space_custody import self_check as custody_self_check
from space_fixture import CensusUnavailable, Fixture
from space_images import Images, self_check as images_self_check
from space_oracles import self_check as oracles_self_check

SCHEMA = "barge.space.native/1"
SUPERVISOR_SHA256 = "fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b"
SUPERVISOR = os.path.expanduser("~/Dev/projects/oxide/scripts/bounded-test.py")
PARENT = "/Volumes/Builds/scratch/barge"
SCRIPT = os.path.abspath(__file__)
CASES = ("1", "2", "3", "4", "4b", "5", "6", "7", "8", "9", "10")
# The first eight scenarios build on prior fixture contents; case 9 requires
# case 6's successful detaches, and case 10 uses its own state and registration
# plan. A separate runtime custody gate protects every continuation.
CASE_NEEDS = {
    "1": (), "2": ("1",), "3": ("2",), "4": ("3",), "4b": ("4",),
    "5": ("4b",), "6": ("5",), "7": ("6",), "8": ("7",),
    "9": ("6",), "10": (),
}
PREMISES = (
    "private HOME, CARGO_HOME and BARGE_STATE_DIR",
    "private account resolution; complete before and after real-state digest",
    "TMPDIR, TMP and TEMP inside the owned root; inherited injection removed",
    "explicit fixture volume and area declarations; no real UUID",
    "two owned 128 MiB images, sequential -owners on attachments and ordinary detach",
    "only the listed proof-only unlinkat interposer and environment hooks",
    "temporary storage inside the supervisor-owned root",
    "fresh mkdtemp fixture directories under the owned root",
    "qualified supervisor pinned by SHA-256; no second supervisor",
)
PREMISE_TRACE = (
    "inner creates private home, cargo home and state and passes them to every child",
    "inner checks fixture registry and bounded real-state digests before and after",
    "inner overrides all temporary variables and removes inherited interposers",
    "Fixture.register saves and applies each exact fixture registration plan",
    "Images publishes both backings before attach and gates ordinary detaches",
    "ENVIRONMENT lists hooks and case 8 injects the proof-only unlinkat shim",
    "inner creates its run directory inside the guard-owned scratch",
    "inner uses mkdtemp under its owned run directory",
    "outer verifies the pinned supervisor and supplies the fixed limits and parent",
)
ENVIRONMENT = {
    "BARGE_PROOF_BIN": "owner-supplied binary under test",
    "BARGE_PROOF_SUPERVISOR": "optional path to the pinned supervisor",
    "OXIDE_PROOF_EVIDENCE": "supervisor evidence and receipts",
    "OXIDE_TASK_SCRATCH_ROOT": "supervisor-owned root",
    "OXIDE_PROOF_DEADLINE_NS": "absolute CLOCK_UPTIME_RAW deadline",
    "HOME": "private fixture account home", "CARGO_HOME": "private fixture cargo home",
    "BARGE_STATE_DIR": "private fixture state", "TMPDIR": "private temporary root",
    "TMP": "private temporary root", "TEMP": "private temporary root",
    "DYLD_INSERT_LIBRARIES": "proof-only unlinkat interposer for case 8",
    "BARGE_PROOF_PAUSE_PATH": "exact case 8 fixture root identity",
    "BARGE_PROOF_PAUSE_LOG": "evidence of the exact paused unlink",
}


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def command(supervisor, mode, binary):
    return [sys.executable, supervisor, "--limit", "300", "--scratch-budget-mib", "512",
            "--root-parent", PARENT, "--", sys.executable, SCRIPT, "--inner", mode,
            "--binary", binary]


def run_cases(diagnostic, execute, ready, check_pool):
    """Run each case whose dependencies and custody are positively established."""
    results = []
    passed = set()
    fault = None
    for name in CASES:
        missing = [need for need in CASE_NEEDS[name] if need not in passed]
        if fault and not diagnostic:
            results.append({"case": name, "status": "skipped", "kind": "latched"})
            continue
        if missing:
            results.append({"case": name, "status": "skipped", "kind": "dependency",
                            "detail": "dependencies did not pass: " + ", ".join(missing)})
            continue
        reason = ready(name)
        if reason is not None:
            results.append({"case": name, "status": "skipped", "kind": "custody",
                            "detail": reason})
            continue
        started = time.monotonic()
        try:
            evidence = execute(name)
            check_pool()
            passed.add(name)
            results.append({"case": name, "status": "passed", "evidence": evidence,
                            "ms": int((time.monotonic() - started) * 1000)})
        except Exception as error:
            fault = fault or name
            results.append({"case": name, "status": "failed", "kind": type(error).__name__,
                            "detail": str(error)[:500], "trace": traceback.format_exc()[-800:]})
    return results, fault


def diagnostic_self_check():
    """Test the actual dispatcher with inert case callbacks and no I/O."""
    problems = []
    seen = set()
    if set(CASE_NEEDS) != set(CASES):
        problems.append("case dependencies do not match the case map")
        return problems
    for name in CASES:
        if len(set(CASE_NEEDS[name])) != len(CASE_NEEDS[name]) or not set(CASE_NEEDS[name]) <= seen:
            problems.append(f"case {name} has duplicate, unknown or unordered dependencies")
        seen.add(name)
    def run(failures, diagnostic=True, unavailable=()):
        called = []
        def execute(name):
            called.append(name)
            if name in failures:
                raise AssertionError("injected fault")
            return {}
        results, fault = run_cases(diagnostic, execute,
                                   lambda name: "unproven custody" if name in unavailable else None,
                                   lambda: None)
        return called, {row["case"]: row for row in results}, fault
    called, results, fault = run({"1", "10"})
    if called != ["1", "10"] or fault != "1" or results["2"]["kind"] != "dependency":
        problems.append("diagnostic did not continue an independent case or latch the first fault")
    called, results, fault = run({"7"})
    if ("8" in called or "9" not in called or "10" not in called or fault != "7"
            or results["8"]["kind"] != "dependency"):
        problems.append("diagnostic did not distinguish dependent and independent cases")
    called, results, fault = run({"1"}, diagnostic=False)
    if called != ["1"] or fault != "1" or results["10"]["kind"] != "latched":
        problems.append("counted mode did not stop after its first fault")
    called, results, _ = run({"1"}, unavailable=("10",))
    if "10" in called or results["10"]["kind"] != "custody":
        problems.append("diagnostic continued without positive custody")
    return problems


def self_check():
    problems = custody_self_check() + images_self_check() + oracles_self_check() + diagnostic_self_check()
    if len(CASES) != 11 or len(set(CASES)) != 11:
        problems.append("case map must contain eleven distinct cases")
    for name in CASES:
        if not callable(getattr(Cases, "case_" + name, None)):
            problems.append("missing dispatch for case " + name)
    methods = {name[5:] for name in dir(Cases) if name.startswith("case_")}
    if methods != set(CASES):
        problems.append("case dispatch and map differ")
    if len(PREMISES) != 9 or len(PREMISE_TRACE) != 9 or SUPERVISOR_SHA256 != "fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b":
        problems.append("premises or supervisor pin are incomplete")
    try:
        if digest(supervisor_path()) != SUPERVISOR_SHA256:
            problems.append("installed supervisor bytes do not match the pin")
    except OSError as error:
        problems.append(f"pinned supervisor is unreadable: {error}")
    if not __debug__:
        problems.append("Python assertions must be enabled for case oracles")
    for mode in ("counted", "diagnostic"):
        invocation = command("/supervisor", mode, "/binary")
        if invocation[2:8] != ["--limit", "300", "--scratch-budget-mib", "512", "--root-parent", PARENT]:
            problems.append("supervisor limits or root changed")
    return problems


def supervisor_path():
    return os.environ.get("BARGE_PROOF_SUPERVISOR", SUPERVISOR)


def outer(mode, binary):
    supervisor = supervisor_path()
    try:
        if digest(supervisor) != SUPERVISOR_SHA256:
            raise ValueError("supervisor bytes do not match the qualified pin")
        if not binary or not os.path.isfile(binary) or not os.access(binary, os.X_OK):
            raise ValueError("BARGE_PROOF_BIN must name an executable")
        if not os.path.isdir(PARENT):
            raise ValueError("designated scratch parent is unavailable")
    except (OSError, ValueError) as error:
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": str(error)}))
        return 125
    env = dict(os.environ)
    for key in ("OXIDE_TASK_SCRATCH_ROOT", "DYLD_INSERT_LIBRARIES", "BARGE_PROOF_PAUSE_PATH",
                "BARGE_PROOF_PAUSE_LOG", "BARGE_PROOF_ENOTEMPTY_PATH", "BARGE_PROOF_ENOTEMPTY_LOG",
                "BARGE_PROOF_ENOTEMPTY_COUNT"):
        env.pop(key, None)
    env["BARGE_PROOF_BIN"] = os.path.abspath(binary)
    env["BARGE_PROOF_SUPERVISOR"] = supervisor
    return subprocess.run(command(supervisor, mode, os.path.abspath(binary)), env=env).returncode


def state_observer():
    # Reuse S7's bounded descriptor-relative real-state observer verbatim.
    path = os.path.join(os.path.dirname(__file__), "s7-maintain-proof.py")
    spec = importlib.util.spec_from_file_location("barge_s7_observer", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.real_state_observation


def inner(mode, binary):
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE", "")
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "")
    guard_root = os.path.dirname(os.path.realpath(scratch))
    if (not os.path.isdir(evidence) or not os.path.isdir(scratch)
            or os.path.basename(scratch) != "scratch"
            or os.path.dirname(guard_root) != os.path.realpath(PARENT)
            or not os.path.basename(guard_root).startswith("oxide-proof.")
            or os.path.realpath(evidence) != os.path.join(guard_root, "evidence")
            or os.path.realpath(os.environ.get("TMPDIR", "")) != os.path.realpath(scratch)):
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "supervisor environment is absent or outside its owned root"}))
        return 125
    if not binary or not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "proof binary is not executable"}))
        return 125
    diagnostic = mode == "diagnostic"
    started_ms = int(time.time() * 1000)
    observe = state_observer()
    before = observe()
    after = None
    results, fault, infrastructure, images, fixture = [], None, None, None, None
    real_home = pwd.getpwuid(os.geteuid()).pw_dir
    pool = None
    try:
        if before["status"] not in ("absent", "complete"):
            raise ValueError("real state could not be observed completely before setup")
        deadline = int(os.environ["OXIDE_PROOF_DEADLINE_NS"]) - 20_000_000_000
        root = tempfile.mkdtemp(prefix="barge-space-", dir=scratch)
        home = tempfile.mkdtemp(prefix="home-", dir=root)
        state = tempfile.mkdtemp(prefix="state-", dir=root)
        cargo = tempfile.mkdtemp(prefix="cargo-", dir=root)
        pool = tempfile.mkdtemp(prefix="internal-pool-", dir=root)
        if os.path.realpath(home) == os.path.realpath(real_home):
            raise ValueError("fixture home equals the real account home")
        env = dict(os.environ)
        for key in ("DYLD_INSERT_LIBRARIES", "BARGE_PROOF_PAUSE_PATH", "BARGE_PROOF_PAUSE_LOG",
                    "BARGE_PROOF_ENOTEMPTY_PATH", "BARGE_PROOF_ENOTEMPTY_LOG", "BARGE_PROOF_ENOTEMPTY_COUNT"):
            env.pop(key, None)
        env.update(HOME=home, CARGO_HOME=cargo, BARGE_STATE_DIR=state, TMPDIR=root, TMP=root, TEMP=root)
        images = Images(root, evidence, env, deadline)
        images.create(0)
        mount = "/Volumes/barge-space-" + uuid.uuid4().hex
        attachment = images.attach(0, mount)
        fixture = Fixture(binary, images, root, env, pool)
        registered = fixture.register(mount, 48 * 1024 * 1024, 24 * 1024 * 1024, "primary-plan.json")
        if registered.lower() != attachment["volume_uuid"]:
            raise ValueError("registered UUID differs from owned image")
        with open(os.path.join(state, "registry.json"), encoding="utf-8") as handle:
            registry = json.load(handle)
        if [row["volume_uuid"].lower() for row in registry["volumes"]] != [registered.lower()]:
            raise ValueError("explicit registration did not land in the fixture state")
        preflight = fixture.custody_preflight()
        cases = Cases(fixture)
        def ready(name):
            if images.unknown or images.children:
                return "image or child custody is unknown"
            # Case 10 uses fresh account state and is independent of the earlier
            # scenarios, but may only operate on the registered primary image.
            if name in ("9", "10"):
                current = images.current
                if (current is None or current["backing"] != images.backings[0]
                        or current["mount"] != fixture.mount
                        or current["volume_uuid"] != fixture.uuid.lower()):
                    return "the registered primary image is not positively attached"
            return None

        def check_pool():
            if os.listdir(pool):
                raise AssertionError("fake internal pool is not empty")

        results, fault = run_cases(diagnostic, lambda name: getattr(cases, "case_" + name)(),
                                   ready, check_pool)
    except CensusUnavailable as error:
        infrastructure = {"check": "product_custody_census", "detail": str(error)[:500]}
        preflight = {"outcome": "infrastructure", "reason": str(error)[:500]}
    except Exception as error:
        fault = fault or "setup"
        preflight = None
        results.append({"case": "setup", "status": "failed", "kind": type(error).__name__,
                        "detail": str(error)[:500], "trace": traceback.format_exc()[-800:]})
    finally:
        after = observe()
    complete = before["status"] in ("absent", "complete") and after["status"] in ("absent", "complete")
    unchanged = complete and before["status"] == after["status"] and before["digest"] == after["digest"]
    safe = images is not None and not images.unknown and not images.children
    qualified = (not diagnostic and not fault and not infrastructure and unchanged and safe
                 and images.current is None and len(results) == len(CASES)
                 and all(row["status"] == "passed" for row in results))
    receipt = {"schema": SCHEMA, "mode": mode, "qualified": qualified,
               "classification": "infrastructure" if infrastructure else ("pass" if qualified else "unqualified"),
               "diagnostic": diagnostic, "started_ms": started_ms,
               "activated_by": "owner, separately for each run",
               "supervisor": {"path": supervisor_path(), "name": "scripts/bounded-test.py",
                              "sha256": SUPERVISOR_SHA256, "limit_seconds": 300, "scratch_budget_mib": 512},
               "binary": {"path": binary, "sha256": digest(binary)}, "premises": PREMISES,
               "premise_trace": [{"premise": i + 1, "disposition": text} for i, text in enumerate(PREMISE_TRACE)],
               "environment": {"hooks": ENVIRONMENT,
                               "child": [] if fixture is not None else ["fixture environment not established"]},
               "preflight": preflight,
               "real_state": {"path": os.path.join(real_home, "Library/Application Support/barge/state"),
                              "before": before, "after": after, "complete": complete, "unchanged": unchanged},
               "image": {"backings": images.backings if images else [], "mib": 128,
                         "events": images.events if images else [], "unknown": not safe},
               "cases": results, "case_evidence": [], "infrastructure": infrastructure,
               "fault": fault, "cancelled": None if safe else "image or child custody unknown"}
    path = os.path.join(evidence, "space-native-receipt.json")
    with open(path, "x", encoding="utf-8") as handle:
        json.dump(receipt, handle, indent=1)
        handle.write("\n")
    print(json.dumps({"schema": SCHEMA, "qualified": qualified, "classification": receipt["classification"],
                      "receipt_path": path, "cases": results}, indent=1))
    if qualified:
        return 0
    if infrastructure:
        return 4
    return 3 if diagnostic else 1


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", nargs="?", choices=("counted", "diagnostic"), default="counted")
    parser.add_argument("--inner", choices=("counted", "diagnostic"))
    parser.add_argument("--binary")
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    options = parser.parse_args(argv)
    problems = self_check()
    if options.self_check or options.plan:
        print(json.dumps({"schema": SCHEMA + ".plan", "cases": CASES, "premises": PREMISES,
                          "premise_trace": PREMISE_TRACE, "dependencies": CASE_NEEDS, "environment": ENVIRONMENT, "supervisor": {"sha256": SUPERVISOR_SHA256,
                          "argv": command("<supervisor>", "counted", "<binary>")},
                          "problems": problems, "ok": not problems}, indent=1))
        return 0 if not problems else 1
    if problems:
        print(json.dumps({"schema": SCHEMA, "ok": False, "problems": problems}))
        return 125
    binary = options.binary or os.environ.get("BARGE_PROOF_BIN", "")
    if options.inner:
        return inner(options.inner, binary)
    return outer(options.mode, binary)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
