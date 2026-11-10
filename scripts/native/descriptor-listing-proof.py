#!/usr/bin/env python3
"""Targeted kernel qualification for Barge's shared descriptor listing.

The test/plan modes do not stage or run a native. Each counted or diagnostic
occurrence is separately owner-activated. One qualified supervisor owns the
300 s / 512 MiB envelope. No image, host census, limit change or installation
is used. Ordinary compilation uses Oxide's managed output pool.
"""

import argparse
import contextlib
import importlib.util
import json
import math
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import time
from unittest import mock

sys.dont_write_bytecode = True
SCRIPT_PATH = os.path.abspath(__file__)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT_PATH)))
sys.path.insert(0, os.path.dirname(SCRIPT_PATH))
import space_images  # noqa: E402


def load_release():
    spec = importlib.util.spec_from_file_location(
        "barge_listing_release", os.path.join(os.path.dirname(SCRIPT_PATH), "release-proof.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


base = load_release()
SCHEMA = "barge.descriptor-listing.native/1"
FEATURE = "proof-descriptor-listing"
DRIVER = "descriptor_listing_proof"
RECEIPT = "descriptor-listing-proof-receipt.json"
LIMIT_SECONDS = 300
SCRATCH_MIB = 512
WORK_STOP_SECONDS = 30
LARGE_OPENS = 5000
SMALL_OPENS = 32
# Standard streams plus a generous fixed implementation margin. This is only
# an admission prerequisite; the kernel's actual count decides every case.
REQUIRED_SOFT_FDS = LARGE_OPENS + 64
BUILD_COMMAND = [
    "oxide", "build", "-p", "barge", "--locked", "--offline", "--features", FEATURE,
    "--example", DRIVER, "--message-format", "json",
]
CASES = [
    {"id": "L1", "opens": SMALL_OPENS, "rows": 65536, "inside": False,
     "claim": "below-threshold complete listing outside the target"},
    {"id": "L2", "opens": LARGE_OPENS, "rows": 65536, "inside": False,
     "claim": "4097-8191 actual descriptors grow and complete outside the target"},
    {"id": "L3", "opens": LARGE_OPENS, "rows": 65536, "inside": True,
     "claim": "the same owned child holds writers inside the target and refuses"},
    {"id": "L4", "opens": LARGE_OPENS, "rows": 4096, "inside": False,
     "claim": "a full available budget is bounded and cannot prove custody"},
]
LIMITATION = (
    "Targeted actual-kernel listing and per-descriptor observation of one owned child only. "
    "This does not qualify a whole-host custody scan, integrated deletion, lifetime closure, "
    "production account lookup, adoption, installation or an atomic descriptor census. "
    "Resource fit is unmeasured until a separately activated native receipt exists."
)


def artifact_from_build(output):
    """Accept exactly the feature-bound example's compiler artifact."""
    found = []
    for line in output.splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        target = row.get("target", {})
        if (row.get("reason") == "compiler-artifact" and target.get("name") == DRIVER
                and target.get("kind") == ["example"] and row.get("executable")):
            found.append({"executable": row["executable"], "features": row.get("features")})
    return found[0] if len(found) == 1 else None


# Compose the accepted release staging routine through its existing globals:
# command, artifact receiver and feature. The helper's allocation, manifest,
# environment, copying, hashing and deadline checks remain the actual code.
base.BUILD_COMMAND = BUILD_COMMAND
base.PROOF_FEATURE = FEATURE
base.executable_from_build = artifact_from_build
release_stage_environment = base.stage_environment


def stage_environment(inherited, tmpdir):
    environment = release_stage_environment(inherited, tmpdir)
    # Keep the accepted ordinary-build route, with all three temporary path
    # variables bound to the selected guard scratch. Host test controls are
    # never a source of staged capabilities.
    environment = {key: value for key, value in environment.items()
                   if not key.startswith("OXIDE_TEST_")}
    environment.update(TMPDIR=tmpdir, TMP=tmpdir, TEMP=tmpdir)
    return environment


base.stage_environment = stage_environment


def raw_now():
    return time.clock_gettime_ns(time.CLOCK_UPTIME_RAW)


def remaining_seconds(deadline_ns, now=None):
    current = raw_now() if now is None else now
    if not isinstance(deadline_ns, int) or isinstance(deadline_ns, bool):
        raise base.Infrastructure("deadline", "the guard supplied no integer raw-uptime deadline")
    remaining = (deadline_ns - current) / 1e9
    if not math.isfinite(remaining) or remaining <= 0 or remaining > LIMIT_SECONDS:
        raise base.Infrastructure("deadline", "the original guard deadline is expired or out of bounds")
    return remaining


def stage_deadline(deadline_ns):
    # Observe the receiver's clock BEFORE deriving remaining in the source
    # clock. Adding that duration is conservative; unlike stamps are never
    # compared or subtracted. The imported staging helper uses monotonic_ns.
    local_start = time.monotonic_ns()
    captured_raw = raw_now()
    remaining_seconds(deadline_ns, now=captured_raw)
    return local_start + (deadline_ns - captured_raw)


def source_binding(repo=REPO_ROOT, runner=subprocess.run):
    def git(*args):
        result = runner(["git", "-C", repo, *args], capture_output=True, text=True, timeout=5)
        if result.returncode != 0:
            raise base.Infrastructure("source", f"git {' '.join(args)} refused")
        return result.stdout.strip()
    head = git("rev-parse", "HEAD")
    tree = git("rev-parse", "HEAD^{tree}")
    if len(head) != 40 or len(tree) != 40 or git("status", "--porcelain"):
        raise base.Infrastructure("source", "source must be a clean exact Git head before staging")
    return {
        "head": head, "tree": tree,
        "lock_sha256": base.sha256_file(os.path.join(repo, "Cargo.lock")),
        "toolchain_sha256": base.sha256_file(os.path.join(repo, "rust-toolchain.toml")),
        "script_sha256": base.sha256_file(SCRIPT_PATH),
        "toolchain": base.TOOLCHAIN_CHANNEL, "feature": FEATURE,
    }


def stage_driver(parent, owned, deadline_ns, runner=None, binding=source_binding):
    before = binding()
    manifest, resources = base.stage_proof_binary(
        parent, owned, deadline_ns=stage_deadline(deadline_ns), runner=runner
    )
    after = binding()
    if before != after or manifest["head"] != before["head"]:
        raise base.Infrastructure("source", "the source binding changed while staging")
    manifest["source_binding"] = before
    manifest["bytes"] = os.path.getsize(manifest["path"])
    with open(os.path.join(os.path.dirname(manifest["path"]), base.STAGE_MANIFEST),
              "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, indent=1)
        handle.write("\n")
    return manifest, resources


def driver_environment(inherited, root):
    """Fixture children are private; the staging build remains ordinary Oxide."""
    rustup = inherited.get("RUSTUP_HOME") or os.path.join(inherited["HOME"], ".rustup")
    environment = {
        key: value for key, value in inherited.items()
        if not key.startswith(("OXIDE_", "XDG_", "BARGE_", "CARGO_", "RUSTUP_"))
        and key not in ("HOME", "RUSTFLAGS", "DYLD_INSERT_LIBRARIES", "TMPDIR", "TMP", "TEMP")
    }
    environment.update({
        "HOME": os.path.join(root, "home"), "CARGO_HOME": os.path.join(root, "cargo"),
        "BARGE_ACCOUNT_HOME": os.path.join(root, "home"),
        "BARGE_STATE_DIR": os.path.join(root, "state"),
        "RUSTUP_HOME": rustup, "RUSTUP_TOOLCHAIN": base.TOOLCHAIN_CHANNEL,
        "RUSTUP_AUTO_INSTALL": "0", "CARGO_NET_OFFLINE": "true",
        "OXIDE_TEST_HOME_FROM_ENV": "1", "TMPDIR": root, "TMP": root, "TEMP": root,
        "BARGE_LISTING_PROOF_ROOT": root,
    })
    # These are the current verified guard's values, reintroduced explicitly
    # after dropping inherited Oxide state. The endpoint binds their layout;
    # the helper stage build separately drops them for ordinary compilation.
    for name in ("OXIDE_TASK_SCRATCH_ROOT", "OXIDE_PROOF_EVIDENCE", "OXIDE_PROOF_DEADLINE_NS"):
        if name in inherited:
            environment[name] = inherited[name]
    return environment


class DirectChildren(space_images.Images):
    """A no-image receiver for the established direct-child custody helper.

    Images.run needs only these fields. Its actual CLOCK_UPTIME_RAW remaining
    calculation, Popen/communicate, child tracking and timeout refusal run
    unchanged. No Images constructor or image method is called here.
    """
    def __init__(self, root, environment, work_deadline_ns, closure_deadline_ns=None):
        self.root = root
        self.env = environment
        self.work_deadline_ns = work_deadline_ns
        self.closure_deadline_ns = work_deadline_ns if closure_deadline_ns is None else closure_deadline_ns
        self.children = set()
        self.unknown = False


def own_limit():
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    adequate = soft == resource.RLIM_INFINITY or soft >= REQUIRED_SOFT_FDS
    return {"soft": soft, "hard": hard, "required_soft": REQUIRED_SOFT_FDS,
            "execution_ready": adequate,
            "missing_prerequisite": None if adequate else
            f"the existing caller RLIMIT_NOFILE soft limit {soft} is below {REQUIRED_SOFT_FDS}; no raising is allowed"}


def check_report(case, report, holder, previous_identity=None):
    """The fixed oracle consumes actual production-endpoint evidence, never copies a scan."""
    identity = report.get("identity")
    if not isinstance(identity, dict) or identity.get("pid") != holder["pid"] \
            or identity.get("effective_uid") != holder["uid"] or not identity.get("start"):
        raise base.Infrastructure("identity", "the kernel did not bind the owned child PID/start/effective UID")
    if previous_identity is not None and identity != previous_identity:
        raise base.Infrastructure("identity", "the held child's bound identity changed between cases")
    attempts = report.get("attempts")
    if not isinstance(attempts, list) or not 1 <= len(attempts) <= 4:
        raise base.Infrastructure("report", "the endpoint did not report the bounded actual attempts")
    work = 0
    for attempt in attempts:
        if (attempt.get("identity_before") != identity or attempt.get("identity_after") != identity
                or not isinstance(attempt.get("rows"), int)
                or not 0 <= attempt["rows"] <= attempt.get("capacity", -1)):
            raise base.Infrastructure("report", "an actual attempt lacks its identity or exact returned-row bound")
        work += attempt["rows"]
    if report.get("returned_row_work") != work or work > case["rows"]:
        raise base.Infrastructure("report", "the endpoint's actual row work does not compose with this budget")
    if not isinstance(report.get("limit_ms"), int) or not 0 < report["limit_ms"] <= 10000:
        raise base.Infrastructure("report", "the endpoint did not retain its original bounded local deadline")
    if case["id"] == "L4":
        passed = (report.get("outcome") == "bounded" and work == 4096
                  and len(attempts) == 1 and attempts[0]["capacity"] == 4096
                  and report.get("custody") == "unknown")
    else:
        count = report.get("final_count", -1)
        passed = (report.get("outcome") == "complete" and count == holder["opened"] + 3
                  and count == attempts[-1]["rows"]
                  and count < attempts[-1]["capacity"]
                  and report.get("capacity") == attempts[-1]["capacity"])
        if case["id"] == "L1":
            passed = passed and count < 4096 and len(attempts) == 1
        else:
            passed = (passed and 4097 <= count <= 8191 and len(attempts) == 2
                      and [row["capacity"] for row in attempts] == [4096, 8192]
                      and attempts[0]["rows"] == 4096)
        if case["inside"]:
            passed = passed and report.get("open_count", 0) >= holder["opened"] \
                and report.get("custody") == "unsafe"
        else:
            passed = passed and report.get("open_count") == 0 and report.get("custody") == "safe_sample"
    return passed, identity


def holder_main(count, path, ready):
    """One owned child, exactly three streams and `count` real vnode descriptors."""
    root = os.environ.get("BARGE_LISTING_PROOF_ROOT", "")
    if (count not in (SMALL_OPENS, LARGE_OPENS) or not root
            or os.path.realpath(root) != root
            or not root.startswith(base.DESIGNATED_SCRATCH + os.sep)
            or os.environ.get("TMPDIR") != root
            or path != os.path.join(root, "inside", "held")
            or ready != os.path.join(root, f"holder-{count}.json")):
        raise base.Infrastructure("holder", "the holder is not bound to the owned fixture")
    descriptors = []
    try:
        for _ in range(count):
            descriptors.append(os.open(path, os.O_RDONLY | os.O_NOFOLLOW))
        with open(ready + ".pending", "x", encoding="utf-8") as handle:
            json.dump({"pid": os.getpid(), "uid": os.geteuid(), "opened": len(descriptors)}, handle)
        # Publication happens only after closing the readiness descriptor, so
        # the actual listing has exactly the three streams plus opened fds.
        os.replace(ready + ".pending", ready)
        sys.stdin.buffer.read(1)
        return 0
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


def start_holder(children, root, count):
    ready = os.path.join(root, f"holder-{count}.json")
    payload = os.path.join(root, "inside", "held")
    log = open(os.path.join(root, f"holder-{count}.log"), "xb")
    child = subprocess.Popen([sys.executable, SCRIPT_PATH, "--holder", str(count), payload, ready],
                             env=children.env, cwd=root, stdin=subprocess.PIPE,
                             stdout=log, stderr=log, close_fds=True)
    children.children.add(child)
    try:
        stop = min(children.work_deadline_ns, raw_now() + 5_000_000_000)
        while raw_now() < stop:
            if child.poll() is not None:
                raise base.Infrastructure("child", f"the holder exited before ready ({child.returncode})")
            try:
                with open(ready, encoding="utf-8") as handle:
                    record = json.load(handle)
            except (FileNotFoundError, ValueError):
                time.sleep(0.01)
                continue
            if record != {"pid": child.pid, "uid": os.geteuid(), "opened": count}:
                raise base.Infrastructure("child", "the holder readiness binding is malformed")
            return child, log, record
        raise base.Infrastructure("child", "the holder did not become ready inside its work allowance")
    except BaseException:
        # The caller's finally owns closure even when readiness faults.
        close_holder(children, child, log)
        raise


def close_holder(children, child, log):
    try:
        if child.stdin is not None:
            child.stdin.close()
        timeout = min(5.0, remaining_seconds(children.closure_deadline_ns))
        if timeout <= 0:
            raise subprocess.TimeoutExpired("holder", timeout)
        code = child.wait(timeout=timeout)
        children.children.discard(child)
        if code != 0:
            raise base.Infrastructure("child", f"the owned holder closed nonzero ({code})")
        return {"pid": child.pid, "exit": code, "reaped": True, "all_descriptors_closed": True}
    except (OSError, subprocess.TimeoutExpired) as error:
        children.unknown = True
        raise base.Infrastructure("custody", f"the owned holder is not positively reaped: {error}") from error
    finally:
        log.close()


def inspect(children, binary, case, holder, root):
    captured_raw = raw_now()
    remaining_seconds(children.work_deadline_ns, now=captured_raw)
    milliseconds = min(10000, (children.work_deadline_ns - captured_raw) // 1_000_000)
    if milliseconds <= 0:
        raise base.Infrastructure("deadline", "the original work stop was reached")
    target = os.path.join(root, "inside" if case["inside"] else "outside")
    children.env["BARGE_LISTING_PROOF_PID"] = str(holder["pid"])
    code, output, error = children.run(
        [binary, "inspect", str(holder["pid"]), target, str(case["rows"]), str(milliseconds)],
        min(12, children.remaining()),
    )
    remaining_seconds(children.work_deadline_ns)
    if code != 0 or len(output) > 65536:
        raise base.Infrastructure("endpoint", f"the endpoint refused or exceeded its report bound ({code}): {error[-400:]!r}")
    try:
        report = json.loads(output)
    except (ValueError, UnicodeError) as error:
        raise base.Infrastructure("endpoint", "the endpoint report is unreadable") from error
    remaining_seconds(children.work_deadline_ns)
    return report


def publish(path, receipt):
    # Failure to record is an execution failure. No successful diagnostic can
    # be reported without its own bounded receipt.
    with open(path, "x", encoding="utf-8") as handle:
        json.dump(receipt, handle, indent=1)
        handle.write("\n")


def guarded_run(mode, scratch, evidence, deadline_ns, route):
    receipt = {"schema": SCHEMA, "mode": mode, "qualified": False, "route": route,
               "cases": [], "first_fault": None, "resources": {}, "closure": None,
               "limitation": LIMITATION, "readiness": own_limit()}
    children = None
    holder = None
    holder_log = None
    started = raw_now()
    try:
        if not receipt["readiness"]["execution_ready"]:
            raise base.Infrastructure("rlimit", receipt["readiness"]["missing_prerequisite"])
        work_deadline = deadline_ns - WORK_STOP_SECONDS * 1_000_000_000
        remaining_seconds(work_deadline)
        stage, stage_resources = stage_driver(scratch, {"scratch": scratch}, work_deadline)
        receipt["stage"] = stage
        receipt["resources"]["stage"] = stage_resources
        root = tempfile.mkdtemp(prefix="listing-proof-", dir=scratch)
        environment = driver_environment(dict(os.environ), root)
        for name in ("home", "cargo", "state", "inside", "outside"):
            os.mkdir(os.path.join(root, name), 0o700)
        with open(os.path.join(root, "inside", "held"), "xb") as handle:
            handle.write(b"owned listing proof\n")
        children = DirectChildren(root, environment, work_deadline, deadline_ns)
        identity = None
        active_count = None
        for case in CASES:
            if receipt["first_fault"] and mode != "diagnostic":
                receipt["cases"].append({"id": case["id"], "status": "skipped",
                                         "reason": "counted run stops on its first safely classified oracle fault"})
                continue
            if raw_now() >= work_deadline:
                raise base.Infrastructure("deadline", "the original 30 s work stop was reached")
            if active_count != case["opens"]:
                if holder is not None:
                    previous, previous_log = holder, holder_log
                    holder = holder_log = None
                    receipt.setdefault("holders", []).append(close_holder(children, previous, previous_log))
                holder, holder_log, holder_record = start_holder(children, root, case["opens"])
                identity = None
                active_count = case["opens"]
            report = inspect(children, stage["path"], case, holder_record, root)
            passed, identity = check_report(case, report, holder_record, identity)
            receipt["cases"].append({"id": case["id"], "status": "passed" if passed else "failed",
                                     "kind": "oracle", "holder": holder_record, "report": report})
            if not passed:
                receipt["first_fault"] = receipt["first_fault"] or case["id"]
        if holder is not None:
            previous, previous_log = holder, holder_log
            holder = holder_log = None
            receipt.setdefault("holders", []).append(close_holder(children, previous, previous_log))
    except (base.Infrastructure, space_images.CustodyUnknown) as error:
        receipt["first_fault"] = receipt["first_fault"] or "infrastructure"
        receipt["infrastructure"] = str(error)
    except Exception as error:
        receipt["first_fault"] = receipt["first_fault"] or "exception"
        receipt["infrastructure"] = f"{type(error).__name__}: {error}"
    finally:
        if holder is not None:
            try:
                receipt.setdefault("holders", []).append(close_holder(children, holder, holder_log))
            except Exception as error:
                receipt["infrastructure"] = f"holder closure: {error}"
        recorded = {case["id"] for case in receipt["cases"]}
        for case in CASES:
            if case["id"] not in recorded:
                receipt["cases"].append({"id": case["id"], "status": "unexecuted",
                                         "reason": "infrastructure, identity, deadline or child custody fault stopped work"})
        receipt["closure"] = {"children_reaped": children is None or not children.children,
                              "custody_known": children is None or (not children.unknown and not children.children),
                              "group_closure": "the supervisor's separate receipt is authoritative"}
        receipt["resources"].update({"elapsed_seconds": (raw_now() - started) / 1e9,
                                    "wall_seconds": LIMIT_SECONDS, "scratch_budget_mib": SCRATCH_MIB,
                                    "work_stop_seconds": WORK_STOP_SECONDS,
                                    "peak_opened_holder_descriptors": LARGE_OPENS,
                                    "image_count": 0})
    all_passed = len(receipt["cases"]) == len(CASES) and all(
        case["status"] == "passed" for case in receipt["cases"]
    )
    receipt["qualified"] = (mode == "counted" and all_passed and not receipt.get("infrastructure")
                            and receipt["closure"]["children_reaped"]
                            and receipt["closure"]["custody_known"])
    receipt_path = os.path.join(evidence, RECEIPT)
    publish(receipt_path, receipt)
    print(json.dumps({"schema": SCHEMA, "qualified": receipt["qualified"],
                      "receipt_path": receipt_path, "cases": receipt["cases"],
                      "closure": receipt["closure"], "first_fault": receipt["first_fault"]}))
    return 0 if all_passed and not receipt.get("infrastructure") else 1


def carried_route():
    try:
        return json.loads(os.environ.get(base.ROUTE_ENV, ""))
    except ValueError:
        return None


def inner_half(mode):
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "")
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE", "")
    try:
        deadline = int(os.environ.get("OXIDE_PROOF_DEADLINE_NS", ""))
        remaining_seconds(deadline)
    except (ValueError, base.Infrastructure):
        deadline = None
    facts = {
        "scratch_isdir": os.path.isdir(scratch), "evidence_isdir": os.path.isdir(evidence),
        "tmpdir_inside_scratch": base.inside_owned(os.environ.get("TMPDIR", ""), scratch),
        "supervisor_problems": base.installed_supervisor_problems(),
        "containment_problems": base.guard_containment_problems(base.guard_containment_facts(scratch, evidence)),
        "route": carried_route(), "deadline_ns": deadline,
    }
    refusals = base.guard_refusals(facts)
    if refusals:
        print(json.dumps({"schema": SCHEMA, "refused": refusals}))
        return 125
    return guarded_run(mode, scratch, evidence, deadline, facts["route"])


def supervisor_command(supervisor, mode):
    return [sys.executable, supervisor, "--limit", str(LIMIT_SECONDS),
            "--scratch-budget-mib", str(SCRATCH_MIB), "--root-parent", base.DESIGNATED_SCRATCH,
            "--", sys.executable, SCRIPT_PATH, "--inner", mode]


def outer_half(mode):
    problems = base.installed_supervisor_problems()
    if problems:
        print(json.dumps({"schema": SCHEMA, "refused": problems}))
        return 125
    facts = base.scratch_route_facts()
    problems = base.scratch_route_problems(facts)
    if problems:
        print(json.dumps({"schema": SCHEMA, "refused": problems}))
        return 125
    supervisor = base.supervisor_path()
    environment = dict(os.environ)
    # Preserve genuine Effigy/Nucleus hooks. Only stale guard and interposer
    # state is removed before the qualified supervisor creates this run.
    for name in ("OXIDE_TASK_SCRATCH_ROOT", "OXIDE_PROOF_EVIDENCE", "OXIDE_PROOF_DEADLINE_NS",
                 "DYLD_INSERT_LIBRARIES", base.WALL_DEADLINE_ENV):
        environment.pop(name, None)
    environment[base.ROUTE_ENV] = json.dumps(base.route_document(facts), separators=(",", ":"))
    environment["BARGE_PROOF_SUPERVISOR"] = supervisor
    return subprocess.run(supervisor_command(supervisor, mode), env=environment).returncode


def premise_dispositions():
    return {
        "private_home_rustup": "driver_environment captures invoking RUSTUP_HOME/HOME before private HOME; stage_driver is ordinary managed Oxide with committed lock, pinned 1.97.1/offline/no auto-install; no fixture compilation",
        "account_home": "driver_environment sets OXIDE_TEST_HOME_FROM_ENV=1 and BARGE_ACCOUNT_HOME to private HOME; endpoint needs no account/store lookup",
        "environment": "driver_environment drops inherited OXIDE_/XDG_/BARGE_/CARGO_/RUSTUP_ state then restores explicit fixture values; outer_half preserves genuine admission context",
        "adoption": "n/a: no fixture checkout build or adoption; only stage_driver's existing ordinary managed Oxide build and its normal output placement",
        "cargo_external_argv": "n/a: no fake Cargo external subcommand; BUILD_COMMAND calls the accepted Oxide build route directly",
        "owned_image": "n/a: no image is created, attached, inspected or detached by any path",
        "compiler_flags": "base.stage_environment scrubs RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS/DYLD_INSERT_LIBRARIES; driver_environment does the same",
        "temporary_storage": "all fixture and staged files use explicit fresh guard scratch parents; TMPDIR/TMP/TEMP are equal in driver children; compiler outputs retain ordinary Oxide placement",
        "macos_mktemp": "no shell mktemp; stage and fixture use tempfile.mkdtemp(dir=explicit_owned_parent); self-check uses only a fresh child of caller TMPDIR",
    }


def plan_document():
    return {"schema": SCHEMA, "activated": False, "readiness": own_limit(),
            "supervisor_sha256": base.SUPERVISOR_SHA256,
            "scratch": {"path": base.DESIGNATED_SCRATCH, "uuid": base.SCRATCH_VOLUME_UUID},
            "bounds": {"wall_seconds": LIMIT_SECONDS, "scratch_mib": SCRATCH_MIB,
                       "work_stop_seconds": WORK_STOP_SECONDS, "images": 0,
                       "holder_opens": [SMALL_OPENS, LARGE_OPENS], "concurrent_holders": 1,
                       "inspections": 4, "total_listing_work_ceiling": 4 * 65536},
            "finite_phase_plan": [
                "0-210 s proposed: fresh locked/offline source-bound stage through ordinary Oxide, including waits; no fit promise",
                "210-255 s proposed: two sequential holders, four fixed kernel observations, each endpoint <=10 s and one report <=64 KiB",
                "255-270 s proposed: ordinary holder EOF/exit/reap and bounded receipt; no work after original deadline minus30s",
                "270-300 s reserved: qualified supervisor's own child/group closure; separate receipt required",
            ],
            "resource_plan": {"fixture_bytes": "one small payload, two holder logs/readiness files, private empty homes; <1 MiB planned",
                              "staged_binary": "one fresh binary+manifest inside guard scratch; actual bytes recorded; no assumed build/RSS fit",
                              "compiler_storage": "ordinary Oxide pool placement, outside the guard's fixture scratch accounting; compiler temp files use guard scratch",
                              "fds": "one holder at a time; 32 then5000 actual file opens plus three standard streams; actual total kernel count decides threshold"},
            "cases": CASES, "premises": premise_dispositions(), "limitation": LIMITATION,
            "remaining_gates": ["independent exact-head source/readiness review", "separately activated adequate uncounted diagnostic",
                                "separately activated counted author and distinct retained-reviewer kernel proofs",
                                "final exact-head judgment and separate supervisor group closure"]}


def self_check():
    """Tiny synthetic files/fake external executors drive the actual apparatus."""
    problems = []
    def check(condition, description):
        if not condition:
            problems.append(description)
    check(base.SUPERVISOR_SHA256 == "fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b", "supervisor pin drift")
    problems += base.installed_supervisor_problems()
    env = driver_environment({"HOME": "/original", "PATH": "/bin", "OXIDE_POOL": "host",
                              "OXIDE_TEST_FAULT": "host", "XDG_STATE_HOME": "host",
                              "CARGO_ENCODED_RUSTFLAGS": "host", "RUSTFLAGS": "host",
                              "NUCLEUS_TICKET": "genuine"}, "/owned")
    check(env["RUSTUP_HOME"] == "/original/.rustup" and env["HOME"] == "/owned/home", "private HOME lost original rustup")
    check(all(name not in env for name in ("OXIDE_POOL", "OXIDE_TEST_FAULT", "XDG_STATE_HOME", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")), "driver environment leaked host controls")
    check(env["NUCLEUS_TICKET"] == "genuine" and env["TMPDIR"] == env["TMP"] == env["TEMP"], "admission or temporary environment drift")
    check(len(premise_dispositions()) == 9, "nine-premise map drift")
    for mode in ("counted", "diagnostic"):
        command = supervisor_command("/supervisor", mode)
        check(command[-2:] == ["--inner", mode] and "--root-parent" in command, "native command bypassed guarded entry")
    # Call the ACTUAL entry without replacing any deciding function. Missing
    # guard inputs refuse before the stage function or any external executor.
    with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(__import__("io").StringIO()):
        check(inner_half("counted") == 125, "unguarded native entry did not refuse")
    with mock.patch.object(time, "monotonic_ns", return_value=4_000_000_000), \
            mock.patch.object(time, "clock_gettime_ns", return_value=100_000_000_000):
        check(stage_deadline(105_000_000_001) == 9_000_000_001, "stage mixed clocks or rounded its integer remaining duration")
        for deadline in (99_000_000_000, 401_000_000_000, None, True):
            try:
                remaining_seconds(deadline)
            except base.Infrastructure:
                pass
            else:
                problems.append("invalid original deadline accepted")
    # The imported helper's REAL receiver/global/clock interfaces, including
    # timeout custody, are driven with fake Popen only (no actual child).
    class FakeChild:
        returncode = 0
        def communicate(self, timeout):
            check(0 < timeout <= 2, "helper timeout escaped original remaining")
            return b"answer", b""
    children = DirectChildren("/owned", env, 102_000_000_000)
    with mock.patch.object(space_images.time, "clock_gettime_ns", return_value=100_000_000_000), \
            mock.patch.object(space_images.subprocess, "Popen", return_value=FakeChild()) as spawn:
        check(children.run(["fake"], 10) == (0, b"answer", b""), "direct-child helper receiver drift")
        check(not children.children and spawn.call_args.kwargs["env"] is env, "direct child was not reaped or environment changed")
    child = FakeChild()
    child.communicate = mock.Mock(side_effect=subprocess.TimeoutExpired("fake", 2))
    with mock.patch.object(space_images.time, "clock_gettime_ns", return_value=100_000_000_000), \
            mock.patch.object(space_images.subprocess, "Popen", return_value=child):
        try:
            children.run(["fake"], 10)
        except space_images.CustodyUnknown:
            check(children.unknown and child in children.children, "unknown child custody was erased")
        else:
            problems.append("child timeout accepted")
    # Closure uses the original guard allowance even after work has stopped;
    # its ordinary EOF/wait/reap path and unknown path are exercised directly.
    class FakeHolder:
        pid = 42
        stdin = mock.Mock()
        def wait(self, timeout):
            check(0 < timeout <= 5, "holder closure escaped original guard allowance")
            return 0
    owned_holder = FakeHolder()
    children = DirectChildren("/owned", env, 99_000_000_000, 105_000_000_000)
    children.children.add(owned_holder)
    holder_log = mock.Mock()
    with mock.patch.object(time, "clock_gettime_ns", return_value=100_000_000_000):
        closure = close_holder(children, owned_holder, holder_log)
    check(closure["reaped"] and not children.children and owned_holder.stdin.close.call_count == 1,
          "ordinary holder closure was not exactly one EOF/reap")
    unknown_holder = FakeHolder()
    unknown_holder.wait = mock.Mock(side_effect=subprocess.TimeoutExpired("holder", 5))
    children.children.add(unknown_holder)
    with mock.patch.object(time, "clock_gettime_ns", return_value=100_000_000_000):
        try:
            close_holder(children, unknown_holder, mock.Mock())
        except base.Infrastructure:
            check(children.unknown and unknown_holder in children.children, "unknown holder custody was discarded")
        else:
            problems.append("unknown holder closure accepted")
    identity = {"pid": 42, "start": 12, "effective_uid": 501}
    holder = {"pid": 42, "uid": 501, "opened": LARGE_OPENS}
    report = {"identity": identity, "outcome": "complete", "final_count": 5003,
              "capacity": 8192, "returned_row_work": 9099, "limit_ms": 10000,
              "custody": "safe_sample", "open_count": 0,
              "attempts": [{"capacity": cap, "rows": rows, "identity_before": identity, "identity_after": identity}
                           for cap, rows in [(4096, 4096), (8192, 5003)]]}
    check(check_report(CASES[1], report, holder)[0], "actual endpoint oracle rejected valid growth")
    check(not check_report(CASES[2], report, holder)[0], "outside sample laundered held-writer refusal")
    malformed = dict(report, returned_row_work=5003)
    try:
        check_report(CASES[1], malformed, holder)
    except base.Infrastructure:
        pass
    else:
        problems.append("discarded full prefix was not charged by report oracle")
    # Drive the actual endpoint-command adapter through the actual imported
    # direct-child helper. Only the external executable is faked.
    child = FakeChild()
    child.communicate = mock.Mock(return_value=(json.dumps(report).encode(), b""))
    children = DirectChildren("/owned", dict(env), 102_000_000_000)
    with mock.patch.object(space_images.time, "clock_gettime_ns", return_value=100_000_000_000), \
            mock.patch.object(space_images.subprocess, "Popen", return_value=child) as spawn:
        observed = inspect(children, "/staged/driver", CASES[1], holder, "/owned")
        check(observed == report and spawn.call_args.args[0] ==
              ["/staged/driver", "inspect", "42", "/owned/outside", "65536", "2000"],
              "production endpoint argv or report adapter drift")
        check(spawn.call_args.kwargs["env"]["BARGE_LISTING_PROOF_PID"] == "42", "endpoint PID was not bound")
    # A final endpoint answer arriving at the original work stop refuses even
    # when its actual direct child succeeded and reaped. Only the external
    # executable and clock are faked; the deciding adapter/helper both run.
    late_child = FakeChild()
    late_child.communicate = mock.Mock(return_value=(json.dumps(report).encode(), b""))
    children = DirectChildren("/owned", dict(env), 102_000_000_000)
    with mock.patch.object(space_images.time, "clock_gettime_ns",
                           side_effect=[100_000_000_000, 100_000_000_000,
                                        100_000_000_000, 102_000_000_000]), \
            mock.patch.object(space_images.subprocess, "Popen", return_value=late_child):
        try:
            inspect(children, "/staged/driver", CASES[-1], holder, "/owned")
        except base.Infrastructure as error:
            check(error.check == "deadline" and not children.children,
                  "late endpoint result lost the original deadline or actual child reap")
        else:
            problems.append("late last-case endpoint result was accepted")
    # Actual stage helper: tiny synthetic artifact, fake build executor,
    # actual allocation/copy/hash/manifest and bound/environment checks.
    parent = tempfile.mkdtemp(prefix="listing-apparatus-", dir=os.environ["TMPDIR"])
    artifact = os.path.join(parent, "synthetic-driver")
    Path(artifact).write_bytes(b"not executable; stage control only\n")
    Path(parent, "Cargo.lock").write_text("synthetic lock\n", encoding="utf-8")
    Path(parent, "rust-toolchain.toml").write_text('[toolchain]\nchannel="1.97.1"\n', encoding="utf-8")
    def fake_git(command, **kwargs):
        answer = {("rev-parse", "HEAD"): "a" * 40,
                  ("rev-parse", "HEAD^{tree}"): "b" * 40,
                  ("status", "--porcelain"): ""}[tuple(command[3:])]
        return subprocess.CompletedProcess(command, 0, answer + "\n", "")
    bind = source_binding(parent, fake_git)
    check(bind["lock_sha256"] == base.sha256_file(os.path.join(parent, "Cargo.lock"))
          and bind["toolchain_sha256"] == base.sha256_file(os.path.join(parent, "rust-toolchain.toml")),
          "source binding lost actual lock/toolchain bytes")
    def dirty_git(command, **kwargs):
        if command[3:] == ["status", "--porcelain"]:
            return subprocess.CompletedProcess(command, 0, " M Cargo.lock\n", "")
        return fake_git(command, **kwargs)
    try:
        source_binding(parent, dirty_git)
    except base.Infrastructure:
        pass
    else:
        problems.append("dirty source binding accepted")
    calls = []
    def fake_build(command, **kwargs):
        calls.append((command, kwargs))
        check(command == BUILD_COMMAND and kwargs["env"]["RUSTUP_AUTO_INSTALL"] == "0", "stage command/environment drift")
        check(0 < kwargs["timeout"] <= 20, "stage timeout escaped original wall")
        row = {"reason": "compiler-artifact", "target": {"name": DRIVER, "kind": ["example"]},
               "features": [FEATURE], "executable": artifact}
        return subprocess.CompletedProcess(command, 0, json.dumps(row), "")
    with mock.patch.object(base, "git_head", return_value="a" * 40):
        manifest, _ = stage_driver(parent, {"fixture": parent}, raw_now() + 20_000_000_000,
                                   runner=fake_build, binding=lambda: source_binding(parent, fake_git))
    check(len(calls) == 1 and manifest["sha256"] == base.sha256_file(artifact)
          and manifest["source_binding"] == bind and manifest["features"] == [FEATURE], "stage manifest lost source/bytes/features")
    check(artifact_from_build(json.dumps({"reason": "compiler-artifact", "target": {"name": "barge", "kind": ["bin"]},
                                          "features": [FEATURE], "executable": artifact})) is None, "wrong production endpoint artifact accepted")
    # Actual bounded publication and error path; no report-writing mock.
    receipt_path = os.path.join(parent, "receipt.json")
    publish(receipt_path, {"qualified": False, "cases": [{"id": "L1", "status": "failed"}]})
    check(json.loads(Path(receipt_path).read_text())["qualified"] is False, "failed receipt became qualified")
    try:
        publish(receipt_path, {})
    except FileExistsError:
        pass
    else:
        problems.append("existing receipt was overwritten")
    # The ACTUAL guarded body must retain preflight failures and all omitted
    # case ids in its receipt. Only the external own-RLIMIT reading is faked;
    # no safety/deciding function or dispatcher is replaced.
    with mock.patch.object(resource, "getrlimit", return_value=(64, 64)), \
            contextlib.redirect_stdout(__import__("io").StringIO()):
        code = guarded_run("diagnostic", parent, parent, raw_now() + 30_000_000_000, {})
    refused = json.loads(Path(parent, RECEIPT).read_text(encoding="utf-8"))
    check(code == 1 and refused["qualified"] is False and refused["first_fault"] == "infrastructure"
          and all(row["status"] == "unexecuted" for row in refused["cases"])
          and [row["id"] for row in refused["cases"]] == [case["id"] for case in CASES]
          and refused["closure"]["children_reaped"], "guarded preflight failure lost its errors/omissions/closure")
    return problems


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic"])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--inner", choices=["counted", "diagnostic"])
    parser.add_argument("--holder", nargs=3, metavar=("COUNT", "PATH", "READY"))
    args = parser.parse_args(argv)
    if args.self_check:
        problems = self_check()
        print(json.dumps({"schema": SCHEMA, "self_check": not problems, "problems": problems}))
        return 1 if problems else 0
    if args.plan:
        print(json.dumps(plan_document(), indent=1))
        return 0
    if args.holder:
        count, path, ready = args.holder
        return holder_main(int(count), path, ready)
    if args.inner:
        return inner_half(args.inner)
    if args.mode:
        return outer_half(args.mode)
    parser.error("name a held native mode, --self-check or --plan")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
