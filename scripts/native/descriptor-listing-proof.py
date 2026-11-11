#!/usr/bin/env python3
"""Targeted kernel qualification for Barge's shared descriptor listing.

The test/plan modes do not stage or run a native. Each counted or diagnostic
occurrence is separately owner-activated. One qualified supervisor owns the
600 s / 512 MiB envelope. No image, host census, limit change or installation
is used. Ordinary compilation uses Oxide's managed output pool.
"""

import argparse
import contextlib
import errno
import importlib.util
import json
import math
import os
from pathlib import Path
import resource
import stat
import subprocess
import sys
import tempfile
import time
import types
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
LIMIT_SECONDS = 600
SCRATCH_MIB = 512
WORK_STOP_SECONDS = 30
CASE_RESERVE_SECONDS = 65
BUILD_LOG_BYTES = 1_048_576
LARGE_OPENS = 5000
SMALL_OPENS = 32
# Standard streams plus a generous fixed implementation margin. This is only
# an admission prerequisite; the kernel's actual count decides every case.
REQUIRED_SOFT_FDS = LARGE_OPENS + 64
MAX_SUPPORTED_SOFT_FDS = 1_048_576
HOLDER_SCHEMA = "barge.descriptor-listing.holder/1"
REPORT_BYTES = 65536
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


def source_binding(repo=REPO_ROOT, runner=None):
    runner = runner or subprocess.run
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


def stage_driver(parent, owned, deadline_ns, attempt=None, binding=source_binding):
    """Keep the accepted stage/manifest producer; adapt only its external executor.

    Partial provenance lives in the caller's receipt even if no binary exists.
    Producer reap never stands for compiler-descendant or group closure.
    """
    attempt = {} if attempt is None else attempt
    attempt.update(schema="barge.listing.stage-attempt/1", status="started",
                   source_before=None, source_after=None, manifest=None, producer=None,
                   build_command=BUILD_COMMAND, toolchain=base.TOOLCHAIN_CHANNEL,
                   feature=FEATURE, deadline_raw_ns=deadline_ns)
    started = raw_now()
    executor = None
    try:
        remaining_seconds(deadline_ns)
        problems = base.stage_parent_problems(parent, owned)
        if problems:
            raise base.Infrastructure("stage", "; ".join(problems))
        before = binding()
        attempt["source_before"] = before
        logs = tempfile.mkdtemp(prefix="listing-stage-logs-", dir=parent)
        executor = StageExecutor(logs, deadline_ns, attempt)
        manifest, resources = base.stage_proof_binary(
            parent, owned, deadline_ns=stage_deadline(deadline_ns), runner=executor.run
        )
        attempt["manifest"] = manifest
        after = binding()
        attempt["source_after"] = after
        problems = base.staged_build_problems(manifest["path"], before["head"], parent)
        if (before != after or manifest["head"] != before["head"]
                or manifest["features"] != [FEATURE] or manifest["toolchain"] != before["toolchain"]
                or manifest["offline"] is not True or problems):
            raise base.Infrastructure("source", "stage source/artifact/manifest binding is invalid")
        remaining_seconds(deadline_ns)
        manifest["source_binding"] = before
        manifest["bytes"] = os.path.getsize(manifest["path"])
        with open(os.path.join(os.path.dirname(manifest["path"]), base.STAGE_MANIFEST),
                  "w", encoding="utf-8") as handle:
            json.dump(manifest, handle, indent=1)
            handle.write("\n")
        remaining_seconds(deadline_ns)
        attempt["status"] = "complete"
        return manifest, resources
    except Exception as error:
        attempt["status"] = "failed"
        attempt["error"] = f"{type(error).__name__}: {error}"[:1024]
        raise
    finally:
        attempt["elapsed_seconds"] = (raw_now() - started) / 1e9
        attempt["producer"] = None if executor is None else executor.record
        attempt["group_closure"] = "Unknown here; qualified supervisor's separate receipt is authoritative"


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


class ProducerSet(set):
    """Observe the actual child added by Images.run, before it can be reaped."""
    def __init__(self, record):
        super().__init__()
        self.record = record
        self.producer = None

    def add(self, child):
        super().add(child)
        self.producer = child
        self.record.update(started=True, pid=child.pid, started_raw_ns=raw_now())
        try:
            parent_group, parent_session = os.getpgrp(), os.getsid(0)
            group, session = os.getpgid(child.pid), os.getsid(child.pid)
            self.record.update(group=group, session=session,
                               parent_group=parent_group, parent_session=parent_session)
            if group != parent_group or session != parent_session:
                raise OSError("producer is outside the inherited qualified group/session")
        except OSError as error:
            raise space_images.CustodyUnknown(f"producer scope is Unknown: {error}") from error
        self.record["scope"] = "same inherited group/session; descendant closure requires guard census"


class StageExecutor:
    """Adapt Images.run to the stage helper's runner interface, without spawning a new scope."""
    def __init__(self, logs, deadline_ns, attempt):
        self.logs = logs
        self.deadline_ns = deadline_ns
        self.attempt = attempt
        self.record = {"started": False, "pid": None, "returncode": None,
                       "reaped": None, "scope": "Unknown", "custody_known": False,
                       "stdout": None, "stderr": None}

    def save_output(self, name, output, complete):
        available = output is not None
        if output is None:
            output = b""
        if not isinstance(output, bytes):
            raise base.Infrastructure("stage", "producer output is not bytes")
        path = os.path.join(self.logs, name + ".log")
        with open(path, "xb") as handle:
            handle.write(output[:BUILD_LOG_BYTES])
        self.record[name] = {"path": path, "bytes": min(len(output), BUILD_LOG_BYTES),
                             "sha256": base.sha256_file(path), "captured_bytes": len(output),
                             "omitted_bytes": max(0, len(output) - BUILD_LOG_BYTES),
                             "available": available, "complete": complete and available}

    def run(self, command, **kwargs):
        if command != BUILD_COMMAND or kwargs["cwd"] != REPO_ROOT:
            raise base.Infrastructure("stage", "unexpected stage producer command or working directory")
        children = DirectChildren(REPO_ROOT, kwargs["env"], self.deadline_ns)
        children.children = ProducerSet(self.record)
        output = error_output = None
        complete = False
        try:
            code, output, error_output = children.run(command, kwargs["timeout"])
            child = children.children.producer
            self.record.update(returncode=child.returncode, reaped=child.returncode is not None,
                               ended_raw_ns=raw_now())
            if child.returncode is None or code != child.returncode:
                children.unknown = True
                raise space_images.CustodyUnknown("producer status/reap is Unknown")
            complete = True
            remaining_seconds(self.deadline_ns)
        except Exception as error:
            # Images.run deliberately leaves a timed-out child tracked and
            # unsignalled. Preserve its actual partial pipe output, not a
            # fabricated successful CompletedProcess or implied group reap.
            cause = error.__cause__
            if isinstance(cause, subprocess.TimeoutExpired):
                output, error_output = cause.output, cause.stderr
                self.record["timed_out"] = True
            if children.children or isinstance(error, space_images.CustodyUnknown):
                children.unknown = True
            self.record["error"] = f"{type(error).__name__}: {error}"[:1024]
            raise
        finally:
            self.record["unreaped_pids"] = [child.pid for child in children.children]
            self.record["custody_known"] = (complete and not children.unknown and not children.children)
            self.save_output("stdout", output, complete)
            self.save_output("stderr", error_output, complete)
        if any(self.record[name]["omitted_bytes"] for name in ("stdout", "stderr")):
            raise base.Infrastructure("stage", "producer output exceeded retained log bounds; artifact refused")
        # This adapts argv/stdout/status only. Custody evidence remains the
        # separately recorded actual Popen/communicate facts above.
        return subprocess.CompletedProcess(command, code, output.decode("utf-8", "strict"),
                                           error_output.decode("utf-8", "strict"))


def own_limit():
    try:
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    except (OSError, ValueError, AttributeError, TypeError, OverflowError) as error:
        return {"soft": None, "hard": None, "required_soft": REQUIRED_SOFT_FDS,
                "maximum_supported_soft": MAX_SUPPORTED_SOFT_FDS,
                "execution_ready": False,
                "missing_prerequisite": f"the existing caller RLIMIT_NOFILE is unreadable: {type(error).__name__}: {error}"}
    supported_soft = (type(soft) is int and soft != resource.RLIM_INFINITY
                      and REQUIRED_SOFT_FDS <= soft <= MAX_SUPPORTED_SOFT_FDS)
    supported_hard = (type(hard) is int and (hard == resource.RLIM_INFINITY or hard >= soft)) \
        if type(soft) is int else False
    adequate = supported_soft and supported_hard
    if not supported_soft:
        missing = (f"the existing caller RLIMIT_NOFILE soft limit {soft} is outside the finite supported "
                   f"range {REQUIRED_SOFT_FDS}..{MAX_SUPPORTED_SOFT_FDS}; no limit change is allowed")
    elif not supported_hard:
        missing = (f"the existing caller RLIMIT_NOFILE hard limit {hard!r} is unreadable or inconsistent "
                   f"with its soft limit {soft}; no limit change is allowed")
    else:
        missing = None
    return {"soft": soft, "hard": hard, "required_soft": REQUIRED_SOFT_FDS,
            "maximum_supported_soft": MAX_SUPPORTED_SOFT_FDS,
            "execution_ready": adequate,
            "missing_prerequisite": missing}


def holder_declarations(record):
    """Validate the fixed holder wire and return its exact declared descriptor set."""
    def refuse(reason):
        raise base.Infrastructure("child", f"the owned holder declaration is invalid: {reason}")
    if not isinstance(record, dict) or set(record) != {
            "schema", "pid", "uid", "opened", "payload_fds", "standard_fds",
            "payload_identity", "soft_limit", "baseline_closed"}:
        refuse("fields")
    if (record["schema"] != HOLDER_SCHEMA or type(record["pid"]) is not int or record["pid"] <= 0
            or type(record["uid"]) is not int or record["uid"] < 0
            or type(record["opened"]) is not int or record["opened"] not in (SMALL_OPENS, LARGE_OPENS)
            or record["baseline_closed"] is not True
            or type(record["soft_limit"]) is not int
            or not REQUIRED_SOFT_FDS <= record["soft_limit"] <= MAX_SUPPORTED_SOFT_FDS):
        refuse("identity, finite limit or baseline closure")
    payload = record["payload_fds"]
    if (not isinstance(payload, list) or len(payload) != record["opened"]
            or any(type(fd) is not int or not 3 <= fd < record["soft_limit"] for fd in payload)
            or payload != sorted(set(payload))):
        refuse("payload descriptor numbers")
    standards = record["standard_fds"]
    if not isinstance(standards, list) or len(standards) != 3:
        refuse("standard stream facts")
    for expected, facts in enumerate(standards):
        if (not isinstance(facts, dict) or set(facts) != {"fd", "kind", "device", "inode", "uid"}
                or type(facts["fd"]) is not int or facts["fd"] != expected
                or facts["kind"] != ("fifo" if expected == 0 else "regular")
                or any(type(facts[key]) is not int or facts[key] < 0 for key in ("device", "inode", "uid"))
                or facts["uid"] != record["uid"]):
            refuse("missing or swapped standard stream")
    if any(standards[1][key] != standards[2][key] for key in ("device", "inode", "uid")):
        refuse("stdout and stderr do not name the same owned log")
    payload_identity = record["payload_identity"]
    if (not isinstance(payload_identity, dict) or set(payload_identity) != {"device", "inode", "uid"}
            or any(type(payload_identity[key]) is not int or payload_identity[key] < 0
                   for key in ("device", "inode", "uid"))
            or payload_identity["uid"] != record["uid"]
            or all(payload_identity[key] == standards[1][key] for key in ("device", "inode"))):
        refuse("payload identity")
    return sorted([facts["fd"] for facts in standards] + payload)


def validate_ready(record, pid, uid, count, log_facts):
    holder_declarations(record)
    if record["pid"] != pid or record["uid"] != uid or record["opened"] != count:
        raise base.Infrastructure("child", "the holder readiness PID/effective UID/count binding changed")
    for facts in record["standard_fds"][1:]:
        if (facts["device"], facts["inode"], facts["uid"]) != (
                log_facts.st_dev, log_facts.st_ino, log_facts.st_uid):
            raise base.Infrastructure("child", "the holder standard streams do not match the parent's owned log")


def check_report(case, report, holder, previous_identity=None):
    """The fixed oracle consumes actual production-endpoint evidence, never copies a scan."""
    declared_fds = holder_declarations(holder)
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
    outcome = report.get("outcome")
    reason = report.get("reason")
    # Diagnostic collection may continue only over a positive, safely
    # classified oracle mismatch. An unknown observation, vanished holder or
    # any deadline is not such a mismatch, even if earlier listing brackets
    # still contain the expected identity.
    if reason == "time":
        raise base.Infrastructure("deadline", "the endpoint exhausted its original observation deadline")
    if outcome not in ("complete", "bounded"):
        raise base.Infrastructure("observation", f"the endpoint observation is {outcome!r}: {reason}")
    if outcome == "bounded" and (case["id"] != "L4" or reason != "descriptor work"):
        raise base.Infrastructure("observation", f"the endpoint reached an unexpected bound: {reason}")
    matching_fds = report.get("matching_fd_numbers")
    if (not isinstance(matching_fds, list)
            or any(type(fd) is not int or not 0 <= fd < holder["soft_limit"] for fd in matching_fds)
            or matching_fds != sorted(set(matching_fds))
            or type(report.get("open_count")) is not int or report["open_count"] != len(matching_fds)):
        raise base.Infrastructure("report", "the endpoint's matching descriptor numbers are malformed")
    if case["id"] == "L4":
        passed = (outcome == "bounded" and reason == "descriptor work" and work == 4096
                  and len(attempts) == 1 and attempts[0]["capacity"] == 4096
                  and report.get("custody") == "unknown"
                  and report.get("fd_numbers") is None and report.get("fd_type_counts") is None
                  and not matching_fds and report["open_count"] == 0)
    else:
        count = report.get("final_count", -1)
        observed_fds = report.get("fd_numbers")
        type_counts = report.get("fd_type_counts")
        if (not isinstance(observed_fds, list)
                or any(type(fd) is not int or not 0 <= fd < holder["soft_limit"] for fd in observed_fds)
                or observed_fds != sorted(set(observed_fds))
                or type(count) is not int or count != len(observed_fds)):
            raise base.Infrastructure("report", "the endpoint's final descriptor numbers are malformed")
        if (not isinstance(type_counts, list) or not type_counts
                or any(not isinstance(pair, list) or len(pair) != 2
                       or type(pair[0]) is not int or not 0 <= pair[0] <= 0xffffffff
                       or type(pair[1]) is not int or pair[1] <= 0 for pair in type_counts)
                or [pair[0] for pair in type_counts] != sorted({pair[0] for pair in type_counts})
                or sum(pair[1] for pair in type_counts) != count):
            raise base.Infrastructure("report", "the endpoint's final descriptor type counts are malformed")
        if not set(matching_fds).issubset(observed_fds):
            raise base.Infrastructure("report", "a matching descriptor is absent from the final listing")
        # An extra or missing descriptor is a residual oracle failure, never a
        # relaxed total or a filtered row. Malformed observations above abort.
        passed = (report.get("outcome") == "complete" and observed_fds == declared_fds
                  and count == len(declared_fds)
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
            passed = passed and report.get("open_count") == holder["opened"] \
                and matching_fds == holder["payload_fds"] \
                and report.get("custody") == "unsafe"
        else:
            passed = passed and not matching_fds and report.get("open_count") == 0 \
                and report.get("custody") == "safe_sample"
    return passed, identity


def standard_stream_facts():
    facts = []
    for expected, stream in enumerate((sys.stdin, sys.stdout, sys.stderr)):
        if type(stream.fileno()) is not int or stream.fileno() != expected:
            raise base.Infrastructure("holder", "the owned holder's standard stream descriptor was swapped")
        metadata = os.fstat(expected)
        expected_kind = stat.S_ISFIFO if expected == 0 else stat.S_ISREG
        if not expected_kind(metadata.st_mode) or metadata.st_uid != os.geteuid():
            raise base.Infrastructure("holder", "the owned holder's standard stream facts are not positively bound")
        facts.append({"fd": expected, "kind": "fifo" if expected == 0 else "regular",
                      "device": metadata.st_dev, "inode": metadata.st_ino, "uid": metadata.st_uid})
    if any(facts[1][key] != facts[2][key] for key in ("device", "inode", "uid")):
        raise base.Infrastructure("holder", "the owned holder's stdout/stderr do not name the same log")
    return facts


def holder_main(count, path, ready):
    """Construct one fresh owned child with a finite, declared descriptor set."""
    root = os.environ.get("BARGE_LISTING_PROOF_ROOT", "")
    if (count not in (SMALL_OPENS, LARGE_OPENS) or not root
            or os.path.realpath(root) != root
            or not root.startswith(base.DESIGNATED_SCRATCH + os.sep)
            or os.environ.get("TMPDIR") != root
            or path != os.path.join(root, "inside", "held")
            or ready != os.path.join(root, f"holder-{count}.json")):
        raise base.Infrastructure("holder", "the holder is not bound to the owned fixture")
    limits = own_limit()
    if not limits["execution_ready"]:
        raise base.Infrastructure("holder", limits["missing_prerequisite"])
    # Only this dedicated fresh --holder child reaches this code. Popen's
    # close_fds=True already removed inherited nonstandard descriptors; after
    # startup we close its own nonstandard range again, without changing the
    # environment, standard streams, a parent or the supervising process.
    initial_standards = standard_stream_facts()
    for descriptor in range(3, limits["soft"]):
        try:
            os.close(descriptor)
        except OSError as error:
            if error.errno != errno.EBADF:
                raise base.Infrastructure("holder", f"own nonstandard descriptor closure failed at {descriptor}: {error}") from error
    payload_metadata = os.stat(path, follow_symlinks=False)
    if not stat.S_ISREG(payload_metadata.st_mode) or payload_metadata.st_uid != os.geteuid():
        raise base.Infrastructure("holder", "the owned payload is not a regular file of this effective UID")
    descriptors = []
    try:
        for _ in range(count):
            descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            if type(descriptor) is not int or descriptor < 3 or descriptor in descriptors:
                raise base.Infrastructure("holder", "the payload open returned a malformed or duplicate descriptor")
            descriptors.append(descriptor)
            if descriptor >= limits["soft"]:
                raise base.Infrastructure("holder", "the payload descriptor exceeds the existing finite soft limit")
        # A marker is created and CLOSED before any final declaration is
        # collected. The record then uses only the already-open stdout log;
        # no temporary readiness descriptor can enter the sampled set.
        with open(ready + ".pending", "xb"):
            pass
        standards = standard_stream_facts()
        if standards != initial_standards:
            raise base.Infrastructure("holder", "the owned holder's standard stream identities changed during setup")
        for descriptor in descriptors:
            metadata = os.fstat(descriptor)
            if (not stat.S_ISREG(metadata.st_mode)
                    or (metadata.st_dev, metadata.st_ino, metadata.st_uid)
                    != (payload_metadata.st_dev, payload_metadata.st_ino, payload_metadata.st_uid)):
                raise base.Infrastructure("holder", "an owned payload descriptor changed identity before readiness")
        record = {"schema": HOLDER_SCHEMA, "pid": os.getpid(), "uid": os.geteuid(),
                  "opened": len(descriptors), "payload_fds": sorted(descriptors),
                  "standard_fds": standards,
                  "payload_identity": {"device": payload_metadata.st_dev,
                                       "inode": payload_metadata.st_ino, "uid": payload_metadata.st_uid},
                  "soft_limit": limits["soft"], "baseline_closed": True}
        holder_declarations(record)
        encoded = json.dumps(record, separators=(",", ":")) + "\n"
        if len(encoded.encode("utf-8")) > REPORT_BYTES:
            raise base.Infrastructure("holder", "the complete holder declaration exceeds its fixed byte bound")
        sys.stdout.write(encoded)
        sys.stdout.flush()
        os.replace(ready + ".pending", ready)
        if sys.stdin.buffer.read(1) != b"":
            raise base.Infrastructure("holder", "ordinary holder closure requires parent EOF, not a data byte")
        return 0
    finally:
        closure_error = None
        for descriptor in descriptors:
            try:
                os.close(descriptor)
            except OSError as error:
                closure_error = closure_error or error
        if closure_error is not None:
            raise base.Infrastructure("holder", f"an owned payload descriptor did not close: {closure_error}") from closure_error


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
            if not os.path.isfile(ready):
                time.sleep(0.01)
                continue
            try:
                marker = os.lstat(ready)
                if (not stat.S_ISREG(marker.st_mode) or marker.st_uid != os.geteuid()
                        or marker.st_size != 0 or os.path.realpath(ready) != ready):
                    raise ValueError("readiness marker is not the empty owned regular file")
                with open(log.name, "rb") as handle:
                    encoded = handle.read(REPORT_BYTES + 1)
                if len(encoded) > REPORT_BYTES:
                    raise ValueError("holder declaration exceeds its complete byte bound")
                record = json.loads(encoded)
            except (OSError, ValueError, UnicodeError) as error:
                raise base.Infrastructure("child", f"the holder readiness declaration is unreadable: {error}") from error
            validate_ready(record, child.pid, os.geteuid(), count, os.fstat(log.fileno()))
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


def publish(path, receipt, deadline_ns=None):
    # Failure to record is an execution failure. No successful diagnostic can
    # be reported without its own bounded receipt.
    with open(path, "x", encoding="utf-8") as handle:
        json.dump(receipt, handle, indent=1)
        handle.write("\n")
        handle.flush()
        if deadline_ns is not None:
            try:
                remaining_seconds(deadline_ns)
            except base.Infrastructure:
                receipt["qualified"] = False
                receipt["first_fault"] = receipt["first_fault"] or "publication"
                receipt["infrastructure"] = "receipt publication exceeded the original work stop"
                handle.seek(0)
                handle.truncate()
                json.dump(receipt, handle, indent=1)
                handle.write("\n")
                raise


def guarded_run(mode, scratch, evidence, deadline_ns, route):
    receipt = {"schema": SCHEMA, "mode": mode, "qualified": False, "route": route,
               "cases": [], "first_fault": None, "resources": {}, "closure": None,
               "limitation": LIMITATION, "readiness": own_limit()}
    children = None
    holder = None
    holder_log = None
    started = raw_now()
    work_deadline = deadline_ns - WORK_STOP_SECONDS * 1_000_000_000
    try:
        if not receipt["readiness"]["execution_ready"]:
            raise base.Infrastructure("rlimit", receipt["readiness"]["missing_prerequisite"])
        remaining_seconds(work_deadline)
        receipt["stage_attempt"] = {}
        stage_stop = work_deadline - CASE_RESERVE_SECONDS * 1_000_000_000
        stage, stage_resources = stage_driver(scratch, {"scratch": scratch}, stage_stop,
                                             attempt=receipt["stage_attempt"])
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
        producer = receipt.get("stage_attempt", {}).get("producer")
        receipt["closure"] = {"children_reaped": None if children is None else not children.children,
                              "custody_known": bool(children is not None and not children.unknown
                                                    and not children.children and producer
                                                    and producer["custody_known"]),
                              "stage_producer": producer,
                              "holders_started": children is not None,
                              "group_closure": "the supervisor's separate receipt is authoritative"}
        receipt["resources"].update({"elapsed_seconds": (raw_now() - started) / 1e9,
                                    "wall_seconds": LIMIT_SECONDS, "scratch_budget_mib": SCRATCH_MIB,
                                    "work_stop_seconds": WORK_STOP_SECONDS,
                                    "planned_maximum_holder_payload_descriptors": LARGE_OPENS,
                                    "image_count": 0})
    all_passed = len(receipt["cases"]) == len(CASES) and all(
        case["status"] == "passed" for case in receipt["cases"]
    )
    receipt["qualified"] = (mode == "counted" and all_passed and not receipt.get("infrastructure")
                            and receipt["closure"]["children_reaped"]
                            and receipt["closure"]["custody_known"])
    receipt_path = os.path.join(evidence, RECEIPT)
    try:
        remaining_seconds(work_deadline)
    except base.Infrastructure as error:
        receipt["qualified"] = False
        receipt["first_fault"] = receipt["first_fault"] or "publication"
        receipt["infrastructure"] = str(error)
    try:
        publish(receipt_path, receipt, work_deadline)
    except Exception as error:
        print(json.dumps({"schema": SCHEMA, "qualified": False, "receipt_path": receipt_path,
                          "publication_error": f"{type(error).__name__}: {error}"[:1024]}))
        return 1
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
        "temporary_storage": "all fixture and staged files use explicit fresh guard scratch parents; TMPDIR/TMP/TEMP are equal in driver children; compiler outputs retain ordinary Oxide placement; only the fresh holder closes its own finite nonstandard range after startup, then records exact payload/standard descriptors after closing the temporary marker; the parent binds the owned log and the endpoint must match that declared set",
        "macos_mktemp": "no shell mktemp; stage and fixture use tempfile.mkdtemp(dir=explicit_owned_parent); self-check uses only a fresh child of caller TMPDIR",
    }


def plan_document():
    limits = own_limit()
    readiness = dict(limits, limit_ready=limits["execution_ready"], execution_ready=False,
                     missing_prerequisite=(limits["missing_prerequisite"] or
                         "fresh source-bound executable/manifest not minted; requires a separately activated in-guard stage"))
    return {"schema": SCHEMA, "activated": False, "readiness": readiness,
            "supervisor_sha256": base.SUPERVISOR_SHA256,
            "scratch": {"path": base.DESIGNATED_SCRATCH, "uuid": base.SCRATCH_VOLUME_UUID},
            "bounds": {"wall_seconds": LIMIT_SECONDS, "scratch_mib": SCRATCH_MIB,
                       "work_stop_seconds": WORK_STOP_SECONDS, "images": 0,
                       "holder_opens": [SMALL_OPENS, LARGE_OPENS], "concurrent_holders": 1,
                       "holder_normalization_close_attempts": 2 * (MAX_SUPPORTED_SOFT_FDS - 3),
                       "holder_readiness_seconds": 5, "case_reserve_seconds": CASE_RESERVE_SECONDS,
                       "retained_build_log_bytes_each": BUILD_LOG_BYTES,
                       "inspections": 4, "total_listing_work_ceiling": 4 * 65536},
            "finite_phase_plan": [
                "0-505 s proposed: fresh locked/offline source-bound stage through ordinary Oxide, including waits; no fit promise",
                "505-555 s proposed: two sequential holders, four fixed kernel observations, each endpoint <=10 s and one report <=64 KiB",
                "555-570 s proposed: ordinary holder EOF/exit/reap and bounded receipt; no work after original deadline minus30s",
                "570-600 s reserved: qualified supervisor's own child/group closure; separate receipt required",
            ],
            "resource_plan": {"fixture_bytes": "two retained build logs <=1 MiB each plus small payload, two holder logs/readiness files and private empty homes; <3 MiB planned",
                              "staged_binary": "one fresh binary+manifest inside guard scratch; actual bytes recorded; no assumed build/RSS fit",
                              "compiler_storage": "ordinary Oxide pool placement outside fixture scratch; inherited direct producer group/session must bind, descendant group closure requires guard census; existing communicate capture is not an RSS bound",
                              "fds": "one fresh holder at a time; finite existing soft limit5064..1048576; two holders perform at most2,097,146 own nonstandard close attempts after startup, then declare actual32/5000 payload fd numbers and positive0/1/2stream facts; exact kernel set must match",
                              "normalization_fit": "fixture setup, not product scan budget; each holder retains the existing5s readiness window; timing/fit unmeasured, no experiment or fit promise"},
            "cases": CASES, "premises": premise_dispositions(), "limitation": LIMITATION,
            "remaining_gates": ["independent exact-head source/readiness review", "separately activated adequate uncounted diagnostic",
                                "separately activated counted author and distinct retained-reviewer kernel proofs",
                                "final exact-head judgment and separate supervisor group closure"]}




def holder_receiver_controls(declaration, check, problems):
    """Drive the actual marker/log receiver and ordinary child cleanup."""
    root = base.DESIGNATED_SCRATCH + "/effect-free-holder"
    log_path = os.path.join(root, f"holder-{SMALL_OPENS}.log")
    ready_path = os.path.join(root, f"holder-{SMALL_OPENS}.json")
    for fault in (None, "nonempty_marker", "symlink_marker", "foreign_marker",
                  "oversize_log", "wrong_log", "invalid_declared_set"):
        candidate = json.loads(json.dumps(declaration))
        if fault == "invalid_declared_set":
            candidate["payload_fds"][-1] = candidate["payload_fds"][-2]
        encoded = (json.dumps(candidate) + "\n").encode("utf-8")
        if fault == "oversize_log":
            encoded = b"x" * (REPORT_BYTES + 1)

        class Log:
            name = log_path
            closes = 0
            def fileno(self):
                return 91
            def close(self):
                self.closes += 1
        log = Log()

        class Reader:
            def __enter__(self):
                return self
            def read(self, size):
                check(size == REPORT_BYTES + 1, "holder readiness log read was not exactly bounded")
                return encoded
            def __exit__(self, *_):
                return False

        class Input:
            closes = 0
            def close(self):
                self.closes += 1

        class Child:
            pid = 42
            stdin = Input()
            waits = 0
            def poll(self):
                return None
            def wait(self, timeout):
                check(0 < timeout <= 5, "receiver child cleanup escaped its original guard remainder")
                self.waits += 1
                return 0
        child = Child()
        environment = {"fixture": "only"}
        children = DirectChildren(root, environment, 102_000_000_000, 105_000_000_000)

        def files(path, mode):
            check(path == log_path, "holder receiver read or wrote outside its own log")
            if mode == "xb":
                return log
            check(mode == "rb", "holder receiver used unexpected log transport flags")
            return Reader()

        marker = types.SimpleNamespace(
            st_mode=(stat.S_IFLNK if fault == "symlink_marker" else stat.S_IFREG) | 0o600,
            st_uid=502 if fault == "foreign_marker" else 501,
            st_size=1 if fault == "nonempty_marker" else 0,
        )
        parent_log = types.SimpleNamespace(st_dev=1, st_ino=999 if fault == "wrong_log" else 22,
                                           st_uid=501)
        failed = None
        with mock.patch("builtins.open", side_effect=files), \
                mock.patch.object(subprocess, "Popen", return_value=child) as spawn, \
                mock.patch.object(os.path, "isfile", return_value=True), \
                mock.patch.object(os.path, "realpath", side_effect=lambda path: path), \
                mock.patch.object(os, "lstat", return_value=marker) as marker_read, \
                mock.patch.object(os, "fstat", return_value=parent_log), \
                mock.patch.object(os, "geteuid", return_value=501), \
                mock.patch.object(time, "clock_gettime_ns", return_value=100_000_000_000):
            try:
                observed_child, observed_log, record = start_holder(children, root, SMALL_OPENS)
            except base.Infrastructure as error:
                failed = error
            if fault is None:
                check(failed is None and observed_child is child and observed_log is log
                      and record == declaration and child in children.children,
                      "actual holder readiness receiver rejected its stable declaration")
                closure = close_holder(children, observed_child, observed_log)
                check(closure["reaped"], "valid receiver control did not ordinarily reap its holder")
            check(spawn.call_args.args[0] == [
                    sys.executable, SCRIPT_PATH, "--holder", str(SMALL_OPENS),
                    os.path.join(root, "inside", "held"), ready_path],
                  "holder receiver changed its fixed child argv")
            bindings = spawn.call_args.kwargs
            check(bindings["env"] is environment and bindings["cwd"] == root
                  and bindings["stdin"] is subprocess.PIPE
                  and bindings["stdout"] is log and bindings["stderr"] is log
                  and bindings["close_fds"] is True,
                  "holder receiver lost direct-child stream/environment/close_fds binding")
            check(marker_read.call_args.args == (ready_path,),
                  "holder receiver did not positively read its own marker")
        check(not children.children and not children.unknown and child.stdin.closes == 1
              and child.waits == 1 and log.closes == 1,
              "holder receiver failure did not preserve exactly one ordinary EOF/wait cleanup")
        if fault is not None:
            check(failed is not None, f"holder receiver accepted injected {fault}")

def holder_controls(check, problems):
    """Actual holder body with only OS/resource/file/stream operations injected."""
    root = base.DESIGNATED_SCRATCH + "/effect-free-holder"
    payload_path = os.path.join(root, "inside", "held")
    ready_path = os.path.join(root, f"holder-{SMALL_OPENS}.json")

    class Fixture:
        def __init__(self, fault=None):
            self.fault = fault
            self.soft = REQUIRED_SOFT_FDS
            self.closed = []
            self.opens = 0
            self.marker_closed = False
            self.flushed = False
            self.published = False
            self.output = ""
            self.fds = {0: self.metadata(stat.S_IFIFO, 11),
                        1: self.metadata(stat.S_IFREG, 22),
                        2: self.metadata(stat.S_IFREG, 22),
                        7: self.metadata(stat.S_IFREG, 77)}

        @staticmethod
        def metadata(kind, inode):
            return types.SimpleNamespace(st_mode=kind | 0o600, st_dev=1,
                                         st_ino=inode, st_uid=501)

        def close(self, descriptor):
            check(type(descriptor) is int and descriptor >= 3,
                  "holder closure touched a standard stream or malformed fd")
            self.closed.append(descriptor)
            if self.fault == "baseline_close" and descriptor == 7 and not self.opens:
                raise OSError(errno.EIO, "synthetic own-close failure")
            if self.fault == "payload_close" and descriptor == 3 and self.published:
                raise OSError(errno.EIO, "synthetic payload-close failure")
            if descriptor not in self.fds:
                raise OSError(errno.EBADF, "synthetic closed descriptor")
            del self.fds[descriptor]

        def open_payload(self, path, flags):
            check(path == payload_path and flags == os.O_RDONLY | os.O_NOFOLLOW,
                  "holder payload open changed its owned path or flags")
            if not self.opens:
                check(self.closed == list(range(3, self.soft)),
                      "holder payload opened before its finite baseline closure completed")
            self.opens += 1
            if self.fault == "duplicate" and self.opens == 2:
                return 3
            if self.fault == "malformed":
                return True
            if self.fault == "out_of_range":
                descriptor = self.soft
            else:
                descriptor = next(fd for fd in range(3, self.soft) if fd not in self.fds)
            self.fds[descriptor] = self.metadata(stat.S_IFREG, 33)
            return descriptor

        def fstat(self, descriptor):
            if self.opens:
                check(self.marker_closed, "final holder facts preceded temporary readiness descriptor closure")
            if (self.fault == "missing_stream" and descriptor == 0
                    or self.fault == "missing_payload" and descriptor == 3 and self.opens):
                raise OSError(errno.EBADF, "synthetic missing descriptor")
            facts = self.fds[descriptor]
            if self.marker_closed and (
                    self.fault == "swapped_payload" and descriptor == 3
                    or self.fault == "changed_stream" and descriptor == 1):
                return self.metadata(stat.S_IFREG, 999)
            return facts

        def marker(self, path, mode):
            check(path == ready_path + ".pending" and mode == "xb",
                  "holder readiness marker escaped its exclusive owned file")
            fixture = self
            class Marker:
                def __enter__(self):
                    self.descriptor = next(fd for fd in range(3, fixture.soft) if fd not in fixture.fds)
                    fixture.fds[self.descriptor] = fixture.metadata(stat.S_IFREG, 44)
                    return self
                def __exit__(self, *_):
                    del fixture.fds[self.descriptor]
                    fixture.marker_closed = True
            return Marker()

        def replace(self, source, destination):
            check(source == ready_path + ".pending" and destination == ready_path
                  and self.marker_closed and self.flushed and self.output.endswith("\n"),
                  "holder readiness was published before descriptor closure or record flush")
            self.published = True

        def stream(self, number):
            fixture = self
            class Stream:
                def fileno(self):
                    return 2 if fixture.fault == "swapped_stream" and number == 1 else number
                def write(self, value):
                    check(number == 1, "holder declaration used a new output descriptor")
                    fixture.output += value
                    return len(value)
                def flush(self):
                    fixture.flushed = True
                def read(self, count):
                    check(number == 0 and count == 1 and fixture.published,
                          "holder wait started before stable readiness")
                    return b"x" if fixture.fault == "non_eof" else b""
            stream = Stream()
            stream.buffer = stream
            return stream

    valid_record = None
    for fault in (None, "missing_stream", "swapped_stream", "changed_stream",
                  "duplicate", "malformed", "out_of_range", "missing_payload",
                  "swapped_payload", "baseline_close", "payload_close", "non_eof"):
        fixture = Fixture(fault)
        failure = None
        with mock.patch.dict(os.environ, {"BARGE_LISTING_PROOF_ROOT": root, "TMPDIR": root}, clear=True), \
                mock.patch.object(resource, "getrlimit", return_value=(fixture.soft, fixture.soft)), \
                mock.patch.object(os.path, "realpath", side_effect=lambda path: path), \
                mock.patch.object(os, "close", side_effect=fixture.close), \
                mock.patch.object(os, "open", side_effect=fixture.open_payload), \
                mock.patch.object(os, "fstat", side_effect=fixture.fstat), \
                mock.patch.object(os, "stat", return_value=fixture.metadata(stat.S_IFREG, 33)), \
                mock.patch.object(os, "geteuid", return_value=501), \
                mock.patch.object(os, "getpid", return_value=42), \
                mock.patch("builtins.open", side_effect=fixture.marker), \
                mock.patch.object(os, "replace", side_effect=fixture.replace), \
                mock.patch.object(sys, "stdin", fixture.stream(0)), \
                mock.patch.object(sys, "stdout", fixture.stream(1)), \
                mock.patch.object(sys, "stderr", fixture.stream(2)):
            try:
                result = holder_main(SMALL_OPENS, payload_path, ready_path)
            except (base.Infrastructure, OSError) as error:
                failure = error
        if fault is None:
            check(failure is None and result == 0 and fixture.published,
                  "actual deterministic holder construction failed")
            check(fixture.closed[:fixture.soft - 3] == list(range(3, fixture.soft)),
                  "holder did not close its exact finite own nonstandard range")
            check(set(fixture.fds) == {0, 1, 2}, "holder payload closure left undeclared descriptors")
            valid_record = json.loads(fixture.output)
            validate_ready(valid_record, 42, 501, SMALL_OPENS, fixture.metadata(stat.S_IFREG, 22))
        else:
            check(failure is not None, f"holder construction accepted injected {fault}")
            check(fixture.published is (fault in ("payload_close", "non_eof")),
                  f"holder published readiness despite injected {fault}")

    for soft in (64, resource.RLIM_INFINITY, MAX_SUPPORTED_SOFT_FDS + 1, True):
        with mock.patch.object(resource, "getrlimit", return_value=(soft, soft)):
            check(not own_limit()["execution_ready"], "unsupported/infinite existing own limit was accepted")
    for soft in (REQUIRED_SOFT_FDS, MAX_SUPPORTED_SOFT_FDS):
        with mock.patch.object(resource, "getrlimit", return_value=(soft, soft)):
            check(own_limit()["execution_ready"], "supported finite own limit was rejected")
    for hard in (REQUIRED_SOFT_FDS - 1, None, "unknown"):
        with mock.patch.object(resource, "getrlimit", return_value=(REQUIRED_SOFT_FDS, hard)):
            limits = own_limit()
            check(not limits["execution_ready"] and "hard limit" in limits["missing_prerequisite"],
                  "an inconsistent/unknown hard limit was misreported as a soft-limit range failure")
    for error in (OSError(errno.EIO, "synthetic missing limit"), ValueError("synthetic invalid limit")):
        with mock.patch.object(resource, "getrlimit", side_effect=error):
            limits = own_limit()
            check(not limits["execution_ready"] and limits["soft"] is None
                  and "unreadable" in limits["missing_prerequisite"],
                  "unreadable own limit did not produce an exact readiness refusal")
    if valid_record is None:
        # No fabricated passing declaration can rescue a failed body control.
        raise AssertionError("the actual holder body produced no valid declaration")
    malformed_records = []
    for change in ("missing", "duplicate", "out_of_range", "malformed", "swapped", "log"):
        candidate = json.loads(json.dumps(valid_record))
        if change == "missing":
            del candidate["standard_fds"]
        elif change == "duplicate":
            candidate["payload_fds"][-1] = candidate["payload_fds"][-2]
        elif change == "out_of_range":
            candidate["payload_fds"][-1] = candidate["soft_limit"]
        elif change == "malformed":
            candidate["payload_fds"][-1] = "not a descriptor"
        elif change == "swapped":
            candidate["standard_fds"][0], candidate["standard_fds"][1] = (
                candidate["standard_fds"][1], candidate["standard_fds"][0])
        else:
            candidate["standard_fds"][1]["inode"] += 1
        malformed_records.append(candidate)
    for candidate in malformed_records:
        try:
            validate_ready(candidate, 42, 501, SMALL_OPENS, Fixture.metadata(stat.S_IFREG, 22))
        except base.Infrastructure:
            pass
        else:
            problems.append("malformed or swapped actual holder readiness declaration was accepted")
    for bindings in ((43, 501, SMALL_OPENS), (42, 502, SMALL_OPENS), (42, 501, LARGE_OPENS)):
        try:
            validate_ready(valid_record, *bindings, Fixture.metadata(stat.S_IFREG, 22))
        except base.Infrastructure:
            pass
        else:
            problems.append("holder readiness lost its PID/effective UID/opened-count binding")
    try:
        validate_ready(valid_record, 42, 501, SMALL_OPENS, Fixture.metadata(stat.S_IFREG, 999))
    except base.Infrastructure:
        pass
    else:
        problems.append("holder readiness accepted another stdout/stderr log")
    holder_receiver_controls(valid_record, check, problems)
    return valid_record

def stage_controls(check, problems):
    """Actual stage/manifest/child helper; fake only external Git/Popen/clock/file reads."""
    parent = tempfile.mkdtemp(prefix="listing-stage-controls-", dir=os.environ["TMPDIR"])
    artifact = os.path.join(parent, "synthetic-artifact")
    Path(artifact).write_bytes(b"inert stage control, never executed\n")
    for fault in (None, "timeout", "nonzero", "wrong_artifact", "wrong_features", "duplicate_artifact",
                  "source_drift", "bad_manifest", "unreaped", "foreign_scope", "spawn",
                  "overflow", "late", "no_time"):
        root = tempfile.mkdtemp(prefix="case-", dir=parent)
        clock = [100_000_000_000]
        drift = [False]
        def fake_git(command, **kwargs):
            check(command[:3] == ["git", "-C", REPO_ROOT], "stage invoked an unexpected external command")
            answer = {("rev-parse", "HEAD"): ("c" if drift[0] else "a") * 40,
                      ("rev-parse", "HEAD^{tree}"): "b" * 40,
                      ("status", "--porcelain"): ""}[tuple(command[3:])]
            return subprocess.CompletedProcess(command, 0, answer + "\n", "")
        row = {"reason": "compiler-artifact", "target": {"name": DRIVER, "kind": ["example"]},
               "features": [FEATURE], "executable": artifact}
        if fault == "wrong_artifact":
            row["target"]["name"] = "barge"
        if fault == "wrong_features":
            row["features"] = []
        output = json.dumps(row).encode()
        if fault == "duplicate_artifact":
            output += b"\n" + output
        if fault == "overflow":
            output += b" " * BUILD_LOG_BYTES
        class Child:
            pid = 42
            returncode = None
            def communicate(self, timeout):
                check(0 < timeout <= 20, "stage child escaped original remaining deadline")
                if fault == "timeout":
                    raise subprocess.TimeoutExpired("oxide", timeout, output=b"partial stdout", stderr=b"partial stderr")
                self.returncode = None if fault == "unreaped" else (7 if fault == "nonzero" else 0)
                drift[0] = fault == "source_drift"
                if fault == "late":
                    clock[0] = 121_000_000_000
                return output, b"build stderr"
        child = Child()
        def spawn(command, **kwargs):
            check(command == BUILD_COMMAND and kwargs["cwd"] == REPO_ROOT
                  and kwargs["env"]["RUSTUP_AUTO_INSTALL"] == "0"
                  and kwargs["env"]["CARGO_NET_OFFLINE"] == "true"
                  and kwargs["env"]["TMPDIR"] == kwargs["env"]["TMP"] == kwargs["env"]["TEMP"],
                  "actual stage command/environment changed")
            check(not kwargs.get("start_new_session") and "preexec_fn" not in kwargs,
                  "stage producer escaped inherited supervision scope")
            if fault == "spawn":
                raise OSError("synthetic producer start failure")
            return child
        original_open = open
        def files(path, mode="r", *args, **kwargs):
            if fault == "bad_manifest" and str(path).endswith(base.STAGE_MANIFEST) and mode == "r":
                return __import__("io").StringIO("{malformed manifest")
            return original_open(path, mode, *args, **kwargs)
        attempt = {}
        failure = None
        with mock.patch.object(subprocess, "run", side_effect=fake_git), \
                mock.patch.object(subprocess, "Popen", side_effect=spawn) as popen, \
                mock.patch.object(os, "getpgrp", return_value=5), \
                mock.patch.object(os, "getpgid", return_value=6 if fault == "foreign_scope" else 5), \
                mock.patch.object(os, "getsid", return_value=5), \
                mock.patch.object(time, "clock_gettime_ns", side_effect=lambda *_: clock[0]), \
                mock.patch("builtins.open", side_effect=files):
            try:
                manifest, resources = stage_driver(root, {"fixture": root},
                    100_000_000_000 if fault == "no_time" else 120_000_000_000, attempt)
            except (base.Infrastructure, space_images.CustodyUnknown, OSError) as error:
                failure = error
        if fault is None:
            check(failure is None and attempt["status"] == "complete"
                  and manifest["sha256"] == base.sha256_file(artifact)
                  and manifest["source_binding"] == attempt["source_before"] == attempt["source_after"]
                  and resources["seconds"] >= 0, "actual stage/manifest success composition failed")
        else:
            check(failure is not None and attempt["status"] == "failed", f"stage accepted injected {fault}")
        producer = attempt["producer"]
        if fault == "no_time":
            check(popen.call_count == 0 and producer is None, "exhausted stage budget spawned a producer")
            continue
        check(attempt["source_before"]["toolchain"] == "1.97.1"
              and len(attempt["source_before"]["lock_sha256"]) == 64,
              "failed stage lost partial tool/source/lock provenance")
        check(producer is not None and producer["custody_known"] is
              (fault not in ("timeout", "unreaped", "foreign_scope", "spawn")),
              f"stage producer custody was fabricated or erased for {fault}")
        if fault in ("timeout", "foreign_scope", "unreaped"):
            check(producer["unreaped_pids"] == ([42] if fault != "unreaped" else [])
                  and producer["returncode"] is None and producer["reaped"] is not True,
                  "Unknown producer was turned into a reaped CompletedProcess")
        for name in ("stdout", "stderr"):
            log = producer[name]
            check(log is not None and log["bytes"] <= BUILD_LOG_BYTES
                  and base.sha256_file(log["path"]) == log["sha256"], "bounded build output was lost")
        if fault == "timeout":
            check(Path(producer["stdout"]["path"]).read_bytes() == b"partial stdout"
                  and Path(producer["stderr"]["path"]).read_bytes() == b"partial stderr"
                  and producer["stdout"]["complete"] is False, "timeout discarded actual partial build output")
        if fault == "overflow":
            check(producer["stdout"]["omitted_bytes"] > 0, "output overflow was silently truncated to an accepted artifact")
    return parent


def guarded_controls(check, problems):
    """Actual entry body/stage/holder/report flow with external producers and kernel reports faked."""
    parent = tempfile.mkdtemp(prefix="listing-flow-controls-", dir=os.environ["TMPDIR"])
    artifact = os.path.join(parent, "synthetic-artifact")
    Path(artifact).write_bytes(b"inert driver, never executed\n")
    for fault in (None, "oracle", "timeout", "nonzero", "unknown", "publication", "late_publication", "time_budget"):
        root = tempfile.mkdtemp(prefix="case-", dir=parent)
        clock = [100_000_000_000]
        holders = {}
        observations = []
        class Child:
            returncode = None
            pid = 42
            stdin = None
            def communicate(self, timeout):
                self.returncode = 0
                if self.command == BUILD_COMMAND:
                    if fault == "timeout":
                        self.returncode = None
                        raise subprocess.TimeoutExpired("oxide", timeout, output=b"stage partial", stderr=b"wait")
                    if fault == "nonzero":
                        self.returncode = 9
                    row = {"reason": "compiler-artifact", "target": {"name": DRIVER, "kind": ["example"]},
                           "features": [FEATURE], "executable": artifact}
                    return json.dumps(row).encode(), b"stage stderr"
                record = holders[int(self.command[2])]
                identity = {"pid": record["pid"], "start": 12, "effective_uid": record["uid"]}
                rows = int(self.command[4]); count = record["opened"] + 3
                bounded = rows == 4096
                inside = self.command[3].endswith("/inside")
                fds = holder_declarations(record)
                attempts = [(4096, 4096)] if bounded else ([(4096, count)] if count < 4096 else [(4096, 4096), (8192, count)])
                report = {"identity": identity, "outcome": "bounded" if bounded else "complete",
                          "reason": "descriptor work" if bounded else None, "final_count": 0 if bounded else count,
                          "capacity": 0 if bounded else attempts[-1][0], "returned_row_work": sum(n for _, n in attempts),
                          "limit_ms": 10000, "open_count": record["opened"] if inside else 0,
                          "matching_fd_numbers": record["payload_fds"] if inside else [],
                          "fd_numbers": None if bounded else fds, "fd_type_counts": None if bounded else [[1, count]],
                          "custody": "unknown" if bounded else ("unsafe" if inside else "safe_sample"),
                          "attempts": [{"capacity": cap, "rows": n, "identity_before": identity, "identity_after": identity}
                                       for cap, n in attempts]}
                if fault == "oracle" and len(observations) == 0:
                    report["capacity"] = 4097
                if fault == "unknown":
                    report["outcome"] = "unreadable"
                observations.append(report)
                return json.dumps(report).encode(), b""
            def poll(self):
                return self.returncode
            def wait(self, timeout):
                check(self.stdin.close.called, "holder was waited without ordinary EOF")
                self.returncode = 0
                return 0
        def spawn(command, **kwargs):
            child = Child(); child.command = command
            if "--holder" in command:
                count, path, ready = command[-3:];count = int(count)
                child.pid = 42 if count == SMALL_OPENS else 43
                child.stdin = types.SimpleNamespace(close=mock.Mock())
                log = os.fstat(kwargs["stdout"].fileno())
                stream = {"kind": "regular", "device": log.st_dev, "inode": log.st_ino, "uid": log.st_uid}
                record = {"schema": HOLDER_SCHEMA, "pid": child.pid, "uid": os.geteuid(), "opened": count,
                          "payload_fds": list(range(3, count + 3)), "soft_limit": REQUIRED_SOFT_FDS,
                          "baseline_closed": True,
                          "standard_fds": [dict(stream, fd=0, kind="fifo", inode=1),
                                           dict(stream, fd=1), dict(stream, fd=2)],
                          "payload_identity": {"device": log.st_dev, "inode": log.st_ino + 1, "uid": log.st_uid}}
                holders[child.pid] = record
                kwargs["stdout"].write((json.dumps(record) + "\n").encode());kwargs["stdout"].flush()
                Path(ready).write_bytes(b"")
            return child
        def git(command, **kwargs):
            value = {("rev-parse", "HEAD"): "a" * 40, ("rev-parse", "HEAD^{tree}"): "b" * 40,
                     ("status", "--porcelain"): ""}[tuple(command[3:])]
            return subprocess.CompletedProcess(command, 0, value + "\n", "")
        original_open = open
        class LateFile:
            def __init__(self, handle): self.handle = handle
            def __enter__(self): return self
            def __exit__(self, *args): return self.handle.__exit__(*args)
            def __getattr__(self, name): return getattr(self.handle, name)
            def flush(self):
                self.handle.flush();clock[0] = 671_000_000_000
        def files(path, mode="r", *args, **kwargs):
            if str(path) == os.path.join(root, RECEIPT):
                if fault == "publication": raise OSError("synthetic receipt write failure")
                handle = original_open(path, mode, *args, **kwargs)
                return LateFile(handle) if fault == "late_publication" else handle
            return original_open(path, mode, *args, **kwargs)
        with mock.patch.object(subprocess, "Popen", side_effect=spawn), \
                mock.patch.object(subprocess, "run", side_effect=git), \
                mock.patch.object(os, "getpgrp", return_value=5), \
                mock.patch.object(os, "getpgid", return_value=5), \
                mock.patch.object(os, "getsid", return_value=5), \
                mock.patch.object(resource, "getrlimit", return_value=(REQUIRED_SOFT_FDS, REQUIRED_SOFT_FDS)), \
                mock.patch.object(time, "clock_gettime_ns", side_effect=lambda *_: clock[0]), \
                mock.patch("builtins.open", side_effect=files), \
                contextlib.redirect_stdout(__import__("io").StringIO()):
            code = guarded_run("diagnostic", root, root,
                               130_000_000_000 if fault == "time_budget" else 700_000_000_000, {})
        if fault == "publication":
            check(code == 1, "failed report publication was accepted")
            continue
        receipt = json.loads(Path(root, RECEIPT).read_text())
        if fault == "time_budget":
            check(code == 1 and "stage_attempt" not in receipt and not holders and not observations
                  and receipt["closure"]["custody_known"] is False,
                  "original exhausted work margin started a stage or fabricated custody")
            continue
        check(receipt["stage_attempt"]["source_before"]["feature"] == FEATURE
              and receipt["stage_attempt"]["deadline_raw_ns"] == 605_000_000_000,
              "actual guarded stage lost source/feature or original deadline-minus-reserves binding")
        if fault in ("timeout", "nonzero"):
            check(code == 1 and all(row["status"] == "unexecuted" for row in receipt["cases"])
                  and not holders and receipt["closure"]["custody_known"] is False
                  and receipt["closure"]["children_reaped"] is None,
                  "stage failure fabricated fixture/producer custody or lost omitted cases")
        elif fault == "unknown":
            check(code == 1 and len(observations) == 1
                  and all(row["status"] == "unexecuted" for row in receipt["cases"]),
                  "Unknown observation continued as an oracle fault")
        else:
            expected = "failed" if fault == "oracle" else "passed"
            check(receipt["cases"][0]["status"] == expected and len(receipt["cases"]) == len(observations) == 4
                  and all(row["status"] == "passed" for row in receipt["cases"][1:]),
                  "actual guarded fixed-case composition or diagnostic keep-going drifted")
            check(receipt["closure"]["custody_known"] is True
                  and len(receipt["holders"]) == 2 and all(h["reaped"] for h in receipt["holders"]),
                  "actual ordinary holder EOF/reap or stage producer facts were lost")
            check(code == (0 if fault is None else 1) and receipt["qualified"] is False,
                  "failed/late diagnostic was qualified or returned a successful result")
        if fault == "late_publication":
            check(receipt["infrastructure"] == "receipt publication exceeded the original work stop",
                  "late last report did not retain its original-deadline failure")


def outer_controls(check):
    """Real outer entry and guard argv, with external disk/metadata/guard executor faked."""
    calls = []
    def execute(command, **kwargs):
        if command == [base.DISKUTIL, "info", "-plist", base.SCRATCH_VOLUME_MOUNT]:
            return subprocess.CompletedProcess(command, 0,
                base.plistlib.dumps({"VolumeUUID": base.SCRATCH_VOLUME_UUID}), b"")
        calls.append((command, kwargs))
        return subprocess.CompletedProcess(command, 125)
    original_lstat = os.lstat
    def metadata(path, *args, **kwargs):
        if path == base.DESIGNATED_SCRATCH:
            return types.SimpleNamespace(st_uid=os.getuid(), st_mode=stat.S_IFDIR | 0o700)
        return original_lstat(path, *args, **kwargs)
    with mock.patch.object(subprocess, "run", side_effect=execute), \
            mock.patch.object(os, "lstat", side_effect=metadata), \
            mock.patch.object(os.path, "realpath", side_effect=lambda path: path), \
            mock.patch.dict(os.environ, {"NUCLEUS_CONTROL": "genuine", "EFFIGY_CONTROL": "genuine",
                                         "OXIDE_PROOF_DEADLINE_NS": "stale"}):
        check(main(["diagnostic"]) == 125, "outer entry replaced actual guard refusal status")
    check(len(calls) == 1, "outer entry retried or bypassed its one guard command")
    command, kwargs = calls[0]
    check(command == supervisor_command(base.supervisor_path(), "diagnostic")
          and command[command.index("--limit") + 1] == "600"
          and command[command.index("--scratch-budget-mib") + 1] == "512",
          "prospective fixed guard tuple drifted")
    check(kwargs["env"]["NUCLEUS_CONTROL"] == kwargs["env"]["EFFIGY_CONTROL"] == "genuine"
          and "OXIDE_PROOF_DEADLINE_NS" not in kwargs["env"]
          and json.loads(kwargs["env"][base.ROUTE_ENV])["volume_uuid"] == base.SCRATCH_VOLUME_UUID,
          "outer entry altered genuine admission context or reused a stale deadline")


def self_check():
    """Tiny synthetic files/fake external executors drive the actual apparatus."""
    problems = []
    def check(condition, description):
        if not condition:
            problems.append(description)
    small_holder = holder_controls(check, problems)
    outer_controls(check)
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
    with mock.patch.object(resource, "getrlimit", return_value=(REQUIRED_SOFT_FDS, REQUIRED_SOFT_FDS)):
        planned = plan_document()
    check(planned["bounds"]["wall_seconds"] == 600 and planned["bounds"]["work_stop_seconds"] == 30
          and planned["readiness"]["limit_ready"] is True
          and planned["readiness"]["execution_ready"] is False
          and "executable/manifest" in planned["readiness"]["missing_prerequisite"],
          "effect-free plan hid the missing fresh executable or reset proof/scanner bounds")
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
        for deadline in (99_000_000_000, 701_000_000_000, None, True):
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
    holder = json.loads(json.dumps(small_holder))
    holder.update(opened=LARGE_OPENS, payload_fds=list(range(3, LARGE_OPENS + 3)))
    declared_fds = holder_declarations(holder)
    report = {"identity": identity, "outcome": "complete", "final_count": 5003,
              "capacity": 8192, "returned_row_work": 9099, "limit_ms": 10000,
              "custody": "safe_sample", "open_count": 0,
              "fd_numbers": declared_fds, "fd_type_counts": [[1, 5002], [6, 1]],
              "matching_fd_numbers": [],
              "attempts": [{"capacity": cap, "rows": rows, "identity_before": identity, "identity_after": identity}
                           for cap, rows in [(4096, 4096), (8192, 5003)]]}
    check(check_report(CASES[1], report, holder)[0], "actual endpoint oracle rejected valid growth")
    check(not check_report(CASES[2], report, holder)[0], "outside sample laundered held-writer refusal")
    # Keep the same complete inventory/declaration while changing only the
    # actual growth transcript. No inventory count can replace these bounds.
    one_attempt = json.loads(json.dumps(report))
    one_attempt["attempts"] = [one_attempt["attempts"][-1]]
    one_attempt["returned_row_work"] = one_attempt["final_count"]
    wrong_growth_capacity = json.loads(json.dumps(report))
    wrong_growth_capacity["attempts"][-1]["capacity"] = 8193
    wrong_growth_capacity["capacity"] = 8193
    short_first_attempt = json.loads(json.dumps(report))
    short_first_attempt["attempts"][0]["rows"] = 4095
    short_first_attempt["returned_row_work"] = 9098
    for adverse_growth in (one_attempt, wrong_growth_capacity, short_first_attempt):
        check(not check_report(CASES[1], adverse_growth, holder)[0],
              "the exact inventory replaced the original fixed growth assertion")
    excessive_attempts = json.loads(json.dumps(report))
    excessive_attempts["attempts"] = [excessive_attempts["attempts"][0]] * 5
    excessive_attempts["returned_row_work"] = 5 * 4096
    try:
        check_report(CASES[1], excessive_attempts, holder)
    except base.Infrastructure:
        pass
    else:
        problems.append("more than four actual listing attempts did not fatally refuse")
    budget_report = dict(report, outcome="bounded", reason="descriptor work", custody="unknown",
                         returned_row_work=4096, final_count=0, capacity=0,
                         fd_numbers=None, fd_type_counts=None,
                         attempts=[report["attempts"][0]])
    check(check_report(CASES[-1], budget_report, holder)[0], "actual row-bound oracle rejected exact work exhaustion")
    for candidate_case, candidate, expected_kind in (
            (CASES[1], dict(report, outcome="unreadable", reason="child identity changed after observation"), "observation"),
            (CASES[1], dict(report, outcome="gone", reason=None), "observation"),
            (CASES[1], dict(report, outcome="unknown", reason="unclassified"), "observation"),
            (CASES[1], dict(report, outcome="unreadable", reason="time"), "deadline"),
            (CASES[1], budget_report, "observation"),
            (CASES[-1], dict(budget_report, reason="time"), "deadline"),
            (CASES[-1], dict(budget_report, reason="unclassified bound"), "observation")):
        try:
            check_report(candidate_case, candidate, holder)
        except base.Infrastructure as error:
            check(error.check == expected_kind, "unknown/deadline report lost its fatal classification")
        else:
            problems.append("unknown, gone, deadline or unexpected bound became a keep-going oracle mismatch")
    malformed = dict(report, returned_row_work=5003)
    try:
        check_report(CASES[1], malformed, holder)
    except base.Infrastructure:
        pass
    else:
        problems.append("discarded full prefix was not charged by report oracle")
    inside_report = dict(report, custody="unsafe", open_count=LARGE_OPENS,
                         matching_fd_numbers=holder["payload_fds"])
    check(check_report(CASES[2], inside_report, holder, identity)[0],
          "the actual held-payload set did not support the fixed inside refusal")
    swapped_match = dict(inside_report, matching_fd_numbers=[0] + holder["payload_fds"][1:])
    check(not check_report(CASES[2], swapped_match, holder, identity)[0],
          "matching count alone replaced the exact held-payload set")
    extra_report = json.loads(json.dumps(report))
    extra_report["fd_numbers"].append(LARGE_OPENS + 3)
    extra_report["final_count"] += 1
    extra_report["attempts"][-1]["rows"] += 1
    extra_report["returned_row_work"] += 1
    extra_report["fd_type_counts"][0][1] += 1
    check(not check_report(CASES[1], extra_report, holder)[0],
          "an extra descriptor was filtered or accepted through a relaxed total")
    # Equal cardinality is not set identity: replacing a single declared or
    # observed fd must fail even when count/type/work/growth remain valid.
    swapped_inventory = dict(report, fd_numbers=declared_fds[:-1] + [LARGE_OPENS + 3])
    check(not check_report(CASES[1], swapped_inventory, holder)[0],
          "equal-sized but swapped observed set was reduced to a count oracle")
    swapped_declaration = json.loads(json.dumps(holder))
    swapped_declaration["payload_fds"][-1] = LARGE_OPENS + 3
    check(not check_report(CASES[1], report, swapped_declaration)[0],
          "equal-sized but swapped declaration was reduced to a count oracle")
    for malformed_report in (
            dict(report, fd_numbers=declared_fds[:-1] + [declared_fds[-2]]),
            dict(report, fd_numbers=declared_fds[:-1] + [holder["soft_limit"]]),
            dict(report, fd_numbers=declared_fds[:-1] + ["malformed"]),
            dict(report, fd_type_counts=[[1, 5002], [1, 1]]),
            dict(report, fd_type_counts=[[1, 5001], [6, 1]]),
            dict(report, matching_fd_numbers=[holder["soft_limit"]], open_count=1)):
        try:
            check_report(CASES[1], malformed_report, holder)
        except base.Infrastructure:
            pass
        else:
            problems.append("malformed descriptor inventory/type/matching declarations were accepted")
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
    parent = stage_controls(check, problems)
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
          and refused["closure"]["children_reaped"] is None
          and refused["closure"]["custody_known"] is False,
          "guarded preflight failure lost its errors/omissions/Unknown custody")
    guarded_controls(check, problems)
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
