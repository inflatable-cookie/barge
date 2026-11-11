#!/usr/bin/env python3
"""Held coordinator proof: one guarded image, fresh driver and bounded cases.

The diagnostic and counted entries require separate prospective activation.
`--plan` and `--self-check` do not build, attach an image or start a child.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import select
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import uuid

# Loading the accepted helpers must not create bytecode beside repository
# sources; every writable proof artifact belongs in the guarded fixture.
sys.dont_write_bytecode = True
SCRIPT = os.path.abspath(__file__)
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT)))


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(os.path.dirname(SCRIPT), filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


journal = load("coordinator_journal_helpers", "journal-proof.py")
image_helpers = load("coordinator_image_helpers", "oxide-outputs-proof.py")
base = image_helpers.base
SCHEMA = "barge.coordinator.native/1"
DRIVER = "operation_coordinator_proof"
FEATURES = ["proof-journal", "proof-synthetic-account"]
LIMIT_SECONDS = 600
SCRATCH_MIB = 512
WORK_MARGIN = 30
IMAGE_MIB = 128
RECEIPT_NAME = "operation-coordinator-proof-receipt.json"
# This declaration is source readiness only. Every native occurrence remains
# held for a separate prospective owner activation at its exact reviewed head.
EXECUTION_READY = True
CASES = ("positive", "owed", "lost-ack", "concurrent", "busy", "cancel",
         "withdraw", "mismatch", "intent-capture", "renamed-replay", "removed",
         "unlink-window", "held-writer", "unknown-writer")
BUILD = ["oxide", "build", "-p", "barge", "--locked", "--offline", "--features",
         ",".join(FEATURES), "--example", DRIVER, "--message-format", "json"]


def external_git(arguments, timeout):
    result = subprocess.run(["git", *arguments], cwd=ROOT, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise ValueError(f"git {' '.join(arguments)} failed: {result.stderr[-200:]}")
    return result.stdout.strip()


def source_identity(deadline_ns):
    def git(arguments):
        return external_git(arguments, budget(deadline_ns, 60))

    if git(["status", "--porcelain"]):
        raise ValueError("the source worktree is not clean")
    files = {
        "lock": "Cargo.lock", "toolchain": "rust-toolchain.toml",
        "driver": f"crates/barge/examples/{DRIVER}.rs", "deleter": "crates/barge/src/delete.rs",
        "journal": "crates/barge/src/operation.rs", "runner": "scripts/native/operation-coordinator-proof.py",
        "image_helper": "scripts/native/oxide-outputs-proof.py", "stage_helper": "scripts/native/release-proof.py",
    }
    return {
        "head": git(["rev-parse", "HEAD"]),
        "tree": git(["rev-parse", "HEAD^{tree}"]),
        "files": {key: journal.external_sha256_file(os.path.join(ROOT, path)) for key, path in files.items()},
        "toolchain": base.TOOLCHAIN_CHANNEL,
        "features": FEATURES,
        "build": BUILD,
    }


def external_process(argv, env, timeout):
    return subprocess.run(argv, cwd=ROOT, env=env, capture_output=True, text=True, timeout=timeout)


def external_guarded(argv, env):
    return subprocess.run(argv, env=env, capture_output=True, text=True)


def external_image(binary, evidence, route, parent, owned, run_root):
    proof = image_helpers.AdapterProof(binary, os.path.join(evidence, RECEIPT_NAME), False,
                                       evidence, route, parent, owned)
    proof.run_root = run_root
    return proof


def attach_with_budget(proof, deadline_ns):
    # Reuse the accepted custody helper, limiting each external image query by
    # the original remaining work window. This never enlarges its own timeout.
    original = base.run
    original_process = subprocess.run

    def bounded_run(*args, **kwargs):
        kwargs["timeout"] = budget(deadline_ns, kwargs.get("timeout", 120))
        return original(*args, **kwargs)

    def bounded_process(*args, **kwargs):
        kwargs["timeout"] = budget(deadline_ns, kwargs.get("timeout", 120))
        return original_process(*args, **kwargs)

    base.run = bounded_run
    subprocess.run = bounded_process
    try:
        proof.attach()
    finally:
        base.run = original
        subprocess.run = original_process


def detach_with_budget(proof, deadline_ns):
    original_process = subprocess.run

    def bounded_process(*args, **kwargs):
        available = remaining(deadline_ns)
        if available <= 0:
            raise ValueError("no guarded wall remains for image closure")
        kwargs["timeout"] = min(available, kwargs.get("timeout", 120))
        return original_process(*args, **kwargs)

    subprocess.run = bounded_process
    try:
        return proof.detach("final")
    finally:
        subprocess.run = original_process


def verified_image_binding(proof, run_root):
    backing = getattr(proof, "backing", None)
    mount = getattr(proof, "mount", None)
    device = getattr(proof, "device", None)
    volume_uuid = getattr(proof, "volume_uuid", None)
    custody = getattr(proof, "custody", None)
    if (not isinstance(backing, str) or not os.path.realpath(backing).startswith(os.path.realpath(run_root) + os.sep)
            or os.path.realpath(backing) != backing or not isinstance(mount, str) or not os.path.isabs(mount)
            or not isinstance(device, str) or not device or not isinstance(custody, dict)
            or custody.get("version") != base.CUSTODY_VERSION or custody.get("backings") != [backing]):
        raise ValueError("the image custody indication or mount/device binding is incomplete")
    info = os.lstat(backing)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError("the image backing is not this account's real file")
    try:
        uuid.UUID(volume_uuid)
    except (ValueError, TypeError, AttributeError) as error:
        raise ValueError("the attached image UUID is invalid") from error
    return {"mib": IMAGE_MIB, "mount": mount, "device": device, "volume_uuid": volume_uuid,
            "backing": backing, "backing_uid": info.st_uid, "custody": custody}


def external_writer_child(binary, case, env, timeout, mount):
    """Bind and reap a real holder and coordinator as direct guarded children."""
    child = subprocess.Popen([binary, case], cwd=ROOT, env=env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                             close_fds=True)
    holder = None
    started = time.monotonic()

    def time_left():
        value = timeout - (time.monotonic() - started)
        if value <= 0:
            raise TimeoutError("the writer case exhausted its original budget")
        return value

    def read_ready(process):
        ready, _, _ = select.select([process.stdout], [], [], time_left())
        if not ready:
            raise TimeoutError("a direct child did not publish readiness")
        line = process.stdout.readline()
        return line, json.loads(line)

    try:
        first, message = read_ready(child)
        payload = message.get("payload")
        root = os.path.realpath(mount)
        if (message.get("case") != case or message.get("stage") != "writer-ready"
                or not isinstance(payload, str) or not os.path.realpath(payload).startswith(root + os.sep)):
            raise ValueError("writer readiness did not bind a payload inside the image")
        facts = os.stat(payload, follow_symlinks=False)
        if not os.path.isfile(payload) or os.path.islink(payload):
            raise ValueError("the writer payload is not a real fixture file")
        holder_env = dict(env, BARGE_PROOF_HOLDER_PATH=payload)
        holder = subprocess.Popen([binary, "__hold"], cwd=ROOT, env=holder_env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, close_fds=True)
        _holder_line, observed = read_ready(holder)
        if (observed.get("stage") != "holder-ready" or observed.get("pid") != holder.pid
                or observed.get("uid") != os.getuid() or observed.get("device") != facts.st_dev
                or observed.get("inode") != facts.st_ino or not isinstance(observed.get("start"), int)
                or not isinstance(observed.get("fd"), int) or observed["fd"] < 0):
            raise ValueError("the direct holder did not bind its real process and file identities")
        out, err = child.communicate(json.dumps(observed) + "\n", timeout=time_left())
        holder_out, holder_err = holder.communicate("release\n", timeout=time_left())
        holder_result = subprocess.CompletedProcess([binary, "__hold"], holder.returncode, holder_out, holder_err)
        verdict(holder_result, "__hold")
        if case == "unknown-writer":
            observations = []
            for line in out.splitlines():
                try:
                    document = json.loads(line)
                except ValueError:
                    continue
                if isinstance(document, dict) and document.get("stage") == "injection":
                    observations.append(document)
            if (len(observations) != 1 or observations[0].get("consumed") is not True
                    or observations[0].get("reader") != "descriptor-path-eio"
                    or any(observations[0].get(key) != observed.get(key)
                           for key in ("pid", "start", "uid", "fd", "device", "inode"))):
                raise ValueError("the labelled read failure did not bind and consume the owned holder")
        result = subprocess.CompletedProcess([binary, case], child.returncode, first + out, err)
        result.proof_holder = {**observed, "reaped": True, "exit": holder.returncode}
        return result
    except Exception:
        for process in (child, holder):
            if process is not None:
                if process.poll() is None:
                    process.kill()  # Only the exact direct children created above.
                    process.communicate()
        raise


def external_receipt(path):
    with open(path, encoding="utf-8") as handle:
        text = handle.read(1_048_577)
    if len(text) > 1_048_576:
        raise ValueError("guard receipt is oversized")
    return json.loads(text)


def remaining(deadline_ns):
    return (deadline_ns - journal.external_clock_ns()) / 1_000_000_000


def budget(deadline_ns, maximum):
    available = remaining(deadline_ns) - WORK_MARGIN
    if available <= 0:
        raise ValueError("the original work margin has been reached")
    return min(available, maximum)


def artifact(result):
    if result.returncode != 0:
        raise ValueError(f"the in-guard build exited {result.returncode}: {result.stderr[-300:]}")
    found = []
    for line in result.stdout.splitlines():
        try:
            document = json.loads(line)
        except ValueError:
            continue
        target = document.get("target") or {}
        if (document.get("reason") == "compiler-artifact" and target.get("name") == DRIVER
                and "example" in (target.get("kind") or []) and document.get("executable")):
            found.append(document)
    if len(found) != 1 or sorted(found[0].get("features", [])) != FEATURES:
        raise ValueError("the build did not identify one driver with exactly the proof features")
    return found[0]["executable"]


def stage(effects, run_root, deadline_ns):
    before = effects.identity(deadline_ns)
    stage_dir = effects.mkdir(run_root, "stage-")
    if not os.path.realpath(stage_dir).startswith(os.path.realpath(run_root) + os.sep):
        raise ValueError("the stage directory escaped the owned run root")
    environment = base.stage_environment(dict(effects.env), stage_dir)
    environment.update({"TMPDIR": stage_dir, "TMP": stage_dir, "TEMP": stage_dir})
    problems = base.stage_environment_problems(environment)
    if problems:
        raise ValueError("; ".join(problems))
    result = effects.process(BUILD, environment, budget(deadline_ns, 360))
    source = artifact(result)
    source_hash = journal.external_sha256_file(source)
    binary = os.path.join(stage_dir, DRIVER)
    shutil.copyfile(source, binary)
    os.chmod(binary, 0o755)
    if (journal.external_sha256_file(source) != source_hash
            or journal.external_sha256_file(binary) != source_hash):
        raise ValueError("the staged artifact changed while copying")
    after = effects.identity(deadline_ns)
    if before != after:
        raise ValueError("the source changed during the in-guard build")
    manifest = {**after, "binary_sha256": journal.external_sha256_file(binary),
                "binary_bytes": os.path.getsize(binary), "binary": binary}
    effects.write(os.path.join(stage_dir, "manifest.json"), json.dumps(manifest, indent=2) + "\n")
    return binary, manifest


def verdict(result, case):
    documents = []
    for line in result.stdout.splitlines():
        try:
            document = json.loads(line)
        except ValueError:
            continue
        if isinstance(document, dict) and document.get("case") == case and "status" in document:
            documents.append(document)
    if len(documents) != 1 or result.returncode != 0 or documents[0].get("status") != "passed":
        raise ValueError(f"case {case} did not return one passing verdict: {result.stderr[-300:]}")
    result_document = documents[0]
    faults = {"intent-capture": "post-rename phase publication failure",
              "renamed-replay": "stop after durable renamed phase",
              "removed": "stop after durable removed phase",
              "unlink-window": "post-unlink removed publication failure",
              "lost-ack": "caller settled before a lost reply"}
    if case in faults:
        result_document["injections"] = [faults[case]]
    if case in ("held-writer", "unknown-writer"):
        holder = getattr(result, "proof_holder", None)
        if not isinstance(holder, dict) or holder.get("reaped") is not True or holder.get("exit") != 0:
            raise ValueError(f"case {case} has no positive direct-holder reap")
        result_document["holder"] = holder
        if case == "unknown-writer":
            result_document["injection"] = "descriptor-path-eio, exact holder descriptor, consumed"
    return result_document


class InnerEffects:
    """Real composition with external process, image, clock and storage seams."""

    @staticmethod
    def from_environment():
        self = InnerEffects()
        self.env = journal.external_environment()
        self.guard = journal.InnerEffects.from_environment().guard_facts
        self.identity = source_identity
        self.process = external_process
        self.writer = external_writer_child
        self.image = external_image
        self.mkdir = lambda parent, prefix: tempfile.mkdtemp(prefix=prefix, dir=parent)
        self.write = journal.external_write_file
        return self


def inner_half(mode, effects):
    receipt = {"schema": SCHEMA, "stage": "inner", "mode": mode, "qualified_inner": False,
               "cases": [], "manifest": None, "image_closure": None}
    scratch, evidence = effects.env.get("OXIDE_TASK_SCRATCH_ROOT"), effects.env.get("OXIDE_PROOF_EVIDENCE")
    evidence_safe = False
    try:
        deadline_ns = int(effects.env[base.WALL_DEADLINE_ENV])
        if not scratch or not evidence:
            raise ValueError("the guarded scratch or evidence directory is missing")
        facts = effects.guard(scratch, evidence)
        problems = journal.guard_containment_problems(facts)
        if problems:
            raise ValueError("; ".join(problems))
        evidence_safe = True
        run_root = effects.mkdir(os.path.realpath(scratch), "coordinator-proof-")
        if not os.path.realpath(run_root).startswith(os.path.realpath(scratch) + os.sep):
            raise ValueError("the run root escaped guarded scratch")
        binary, manifest = stage(effects, run_root, deadline_ns)
        receipt["manifest"] = manifest
        if journal.external_sha256_file(binary) != manifest["binary_sha256"]:
            raise ValueError("the staged driver changed before use")
        proof = effects.image(binary, evidence, {}, scratch, {"scratch": scratch}, run_root)
        try:
            budget(deadline_ns, 1)
            attach_with_budget(proof, deadline_ns)
            receipt["image"] = verified_image_binding(proof, run_root)
            failed = False
            for case in CASES:
                if remaining(deadline_ns) <= WORK_MARGIN:
                    receipt["cases"].append({"case": case, "status": "unexecuted",
                                             "detail": "the fixed work margin was reached"})
                    continue
                if failed and mode == "counted":
                    receipt["cases"].append({"case": case, "status": "skipped"})
                    continue
                try:
                    timeout = budget(deadline_ns, 60)
                    case_dir = effects.mkdir(run_root, f"case-{case}-")
                    if not os.path.realpath(case_dir).startswith(os.path.realpath(run_root) + os.sep):
                        raise ValueError("the case directory escaped the owned run root")
                    env = base.stage_environment(dict(effects.env), case_dir)
                    for name in list(env):
                        if name.startswith(("BARGE_", "OXIDE_", "XDG_")):
                            env.pop(name)
                    env.update(proof.case_environment(case_dir))
                    result = (effects.writer(binary, case, env, timeout, proof.mount)
                              if case in ("held-writer", "unknown-writer") else effects.process([binary, case], env, timeout))
                    receipt["cases"].append(verdict(result, case))
                except Exception as error:  # bounded case failure, never a pass
                    receipt["cases"].append({"case": case, "status": "failed", "detail": str(error)[:400]})
                    failed = True
            receipt["image_closure"] = detach_with_budget(proof, deadline_ns)
        except Exception:
            # An interrupted or uncertain attach is retained. No detach or retry.
            receipt["image_closure"] = {"closed": False, "custody": "retained"}
            raise
        finally:
            if "image" not in receipt:
                receipt["image"] = {"mib": IMAGE_MIB, "mount": getattr(proof, "mount", None),
                                    "device": getattr(proof, "device", None),
                                    "volume_uuid": getattr(proof, "volume_uuid", None),
                                    "backing": getattr(proof, "backing", None),
                                    "custody": getattr(proof, "custody", None)}
        closure = receipt["image_closure"] or {}
        receipt["qualified_inner"] = (mode == "counted" and len(receipt["cases"]) == len(CASES)
            and all(case.get("status") == "passed" for case in receipt["cases"])
            and closure.get("closed") is True and closure.get("backing_attached") is False)
    except Exception as error:
        receipt["fault"] = str(error)[:500]
    seen = {case.get("case") for case in receipt["cases"]}
    receipt["cases"].extend({"case": case, "status": "unexecuted",
                             "detail": "setup or staging did not complete"}
                            for case in CASES if case not in seen)
    if evidence_safe:
        try:
            path = os.path.join(os.path.realpath(evidence), RECEIPT_NAME)
            effects.write(path, json.dumps(receipt, indent=2) + "\n")
            receipt["receipt_path"] = path
        except Exception as error:
            receipt["qualified_inner"] = False
            receipt["receipt_fault"] = str(error)[:300]
    return receipt


def group_closed(receipt):
    join = receipt.get("census_join") or {}
    outcome = receipt.get("outcome") or {}
    empty = {"certain": True, "members": 0, "uncertain": 0}
    return (outcome.get("closure") == "closed" and outcome.get("launched") is True
            and outcome.get("fault") is None and outcome.get("deadline") is False
            and join.get("joined") is True and join.get("deciding") == empty
            and join.get("final") == empty)


def outer_half(mode):
    effects = journal.OuterEffects.from_environment()
    result = {"schema": SCHEMA, "stage": "outer", "mode": mode, "qualified": False}
    if effects.supervisor_digest() != base.SUPERVISOR_SHA256:
        result["fault"] = "the qualified supervisor digest differs"
        return result
    problems = journal.designated_route_problems(effects.route_facts())
    if problems:
        result["fault"] = "; ".join(problems)
        return result
    env = dict(effects.env)
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env[base.WALL_DEADLINE_ENV] = str(effects.monotonic_ns() + (LIMIT_SECONDS - 2) * 1_000_000_000)
    env["BARGE_PROOF_SUPERVISOR"] = base.supervisor_path()
    command = [sys.executable, base.supervisor_path(), "--limit", str(LIMIT_SECONDS),
               "--scratch-budget-mib", str(SCRATCH_MIB), "--root-parent", base.DESIGNATED_SCRATCH,
               "--", sys.executable, SCRIPT, "--inner", mode]
    guarded = effects.run_guarded(command, env)
    inner = []
    for line in guarded.stdout.splitlines():
        try:
            document = json.loads(line)
        except ValueError:
            continue
        if isinstance(document, dict) and document.get("schema") == SCHEMA and document.get("stage") == "inner":
            inner.append(document)
    if len(inner) != 1:
        result["fault"] = "the guard did not return exactly one inner receipt"
        return result
    result["inner"] = inner[0]
    path = inner[0].get("receipt_path")
    if not isinstance(path, str) or not os.path.realpath(path).startswith(base.DESIGNATED_SCRATCH + os.sep):
        result["fault"] = "the inner receipt path is outside the designated scratch"
        return result
    try:
        guard_receipt = external_receipt(os.path.join(os.path.dirname(path), "receipt.json"))
    except Exception as error:
        result["fault"] = f"the guard receipt is unavailable: {error}"
        return result
    expected_child = [sys.executable, SCRIPT, "--inner", mode]
    if (guard_receipt.get("command") != expected_child
            or guard_receipt.get("limit_s") != LIMIT_SECONDS
            or (guard_receipt.get("owned_bytes") or {}).get("budget") != SCRATCH_MIB * 1024 * 1024
            or guard_receipt.get("scratch_owned") is not True
            or guard_receipt.get("evidence") != os.path.dirname(path)):
        result["fault"] = "the guard receipt does not bind this command, budget and evidence directory"
        return result
    result["group_closed"] = group_closed(guard_receipt)
    result["guard_receipt"] = os.path.join(os.path.dirname(path), "receipt.json")
    result["qualified"] = (mode == "counted" and guarded.returncode == 0
                           and (guard_receipt.get("outcome") or {}).get("status") == 0
                           and inner[0].get("qualified_inner") is True and result["group_closed"])
    return result


def composition_controls():
    """Drive both real entries and factories with only external seams faked.

    The control owns one fresh directory below the caller's TMPDIR. It never
    creates an image, starts a process, stages a real binary or deletes a
    fixture. A fake compiler artifact still goes through the actual artifact
    receiver, copy, hashing, source recheck and manifest composition.
    """
    from unittest.mock import patch

    if len(CASES) > 16 or len(set(CASES)) != len(CASES):
        raise ValueError("the case map is duplicated or exceeds its reviewed bound")
    driver_source = os.path.join(ROOT, "crates", "barge", "examples", DRIVER + ".rs")
    with open(driver_source, encoding="utf-8") as handle:
        source = handle.read()
    if any(f'"{case}"' not in source for case in CASES):
        raise ValueError("the case map names a missing driver case")
    parent = os.environ.get("TMPDIR")
    if not parent or not os.path.isdir(parent):
        raise ValueError("the caller's fixture TMPDIR is missing")
    fixture = tempfile.mkdtemp(prefix="coordinator-controls-", dir=parent)
    os.chmod(fixture, 0o700)
    root = os.path.join(fixture, "owned")
    scratch = os.path.join(root, "scratch")
    evidence = os.path.join(root, "evidence")
    os.mkdir(root, 0o700)
    os.mkdir(scratch, 0o755)
    os.mkdir(evidence, 0o755)
    os.chmod(root, 0o700)
    os.chmod(scratch, 0o755)
    os.chmod(evidence, 0o755)
    fake_binary = os.path.join(fixture, "compiler-artifact")
    with open(fake_binary, "wb") as handle:
        handle.write(b"fake external compiler artifact; never executable\n")
    calls = {"stage": 0, "cases": 0, "writer": 0, "attach": 0, "detach": 0}
    options = {"stage_ok": True, "attach_ok": True, "image_closed": True,
               "group_closed": True, "case_ok": True}
    normal_env = dict(os.environ)
    guard_env = dict(normal_env, OXIDE_TASK_SCRATCH_ROOT=scratch, OXIDE_PROOF_EVIDENCE=evidence)
    route = {"path": fixture, "realpath": fixture, "present": True, "volume_uuid": journal.ROUTE_UUID,
             "uid": os.getuid(), "owner_uid": os.getuid(), "mode": 0o700}

    class FakeImage:
        mount = fixture
        device = "/dev/fake-proof"
        volume_uuid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"

        def __init__(self, run_root):
            self.backing = os.path.join(run_root, "fake-backing")
            with open(self.backing, "wb") as handle:
                handle.write(b"fixture")
            self.custody = {"version": base.CUSTODY_VERSION, "backings": [self.backing]}

        def attach(self):
            calls["attach"] += 1
            if not options["attach_ok"]:
                raise ValueError("injected unknown image custody")

        def detach(self, _purpose):
            calls["detach"] += 1
            return {"closed": options["image_closed"], "backing_attached": not options["image_closed"]}

        def case_environment(self, case_dir):
            home, state = os.path.join(case_dir, "home"), os.path.join(case_dir, "state")
            os.mkdir(home)
            os.mkdir(state)
            return {"HOME": home, "BARGE_ACCOUNT_HOME": home, "BARGE_STATE_DIR": state,
                    "BARGE_PROOF_VOLUME_UUID": self.volume_uuid, "BARGE_PROOF_VOLUME_MOUNT": self.mount,
                    "TMPDIR": case_dir, "TMP": case_dir, "TEMP": case_dir,
                    "OXIDE_TASK_SCRATCH_ROOT": scratch, "OXIDE_PROOF_EVIDENCE": evidence}

    def process(command, _env, _timeout):
        if command == BUILD:
            calls["stage"] += 1
            if not (_env.get("TMPDIR") == _env.get("TMP") == _env.get("TEMP")
                    and os.path.realpath(_env["TMPDIR"]).startswith(scratch + os.sep)):
                raise ValueError("stage temporary variables escaped guarded scratch")
            artifact_line = {"reason": "compiler-artifact", "target": {"name": DRIVER, "kind": ["example"]},
                             "features": FEATURES, "executable": fake_binary}
            return subprocess.CompletedProcess(command, 0, json.dumps(artifact_line) if options["stage_ok"] else "", "")
        calls["cases"] += 1
        name = command[1] if options["case_ok"] else "foreign-case"
        return subprocess.CompletedProcess(command, 0, json.dumps({"case": name, "status": "passed"}), "")

    def writer(binary, case, env, timeout, mount):
        calls["writer"] += 1
        result = process([binary, case], env, timeout)
        result.proof_holder = {"reaped": True, "exit": 0}
        return result

    def git(arguments, _timeout):
        if arguments == ["status", "--porcelain"]:
            return ""
        return "a" * 40 if arguments == ["rev-parse", "HEAD"] else "b" * 40

    def guarded(_command, env):
        inner_env = dict(guard_env)
        inner_env[base.WALL_DEADLINE_ENV] = env[base.WALL_DEADLINE_ENV]
        with patch.object(journal, "external_environment", return_value=inner_env):
            inner = inner_half("counted", InnerEffects.from_environment())
        empty = {"certain": True, "members": 0, "uncertain": 0}
        join = {"joined": options["group_closed"], "deciding": empty, "final": empty}
        with open(os.path.join(evidence, "receipt.json"), "w", encoding="utf-8") as handle:
            json.dump({"outcome": {"closure": "closed", "launched": True, "fault": None,
                                   "deadline": False, "status": 0}, "census_join": join,
                       "command": [sys.executable, SCRIPT, "--inner", "counted"],
                       "limit_s": LIMIT_SECONDS, "owned_bytes": {"budget": SCRATCH_MIB * 1024 * 1024},
                       "scratch_owned": True, "evidence": evidence}, handle)
        return subprocess.CompletedProcess(_command, 0 if inner.get("qualified_inner") else 1,
                                           json.dumps(inner), "")

    actual_writer = external_writer_child
    patches = [
        patch.object(journal, "DESIGNATED_SCRATCH", fixture),
        patch.object(base, "DESIGNATED_SCRATCH", fixture),
        patch.object(journal.base, "scratch_route_facts", return_value={"volume_uuid": journal.ROUTE_UUID}),
        patch.object(journal, "external_environment", return_value=normal_env),
        patch(__name__ + ".external_git", side_effect=git),
        patch.object(journal, "external_supervisor_digest", return_value=base.SUPERVISOR_SHA256),
        patch.object(journal, "external_route_facts", return_value=route),
        patch.object(journal, "external_run_guarded", side_effect=guarded),
        patch(__name__ + ".external_process", side_effect=process),
        patch(__name__ + ".external_writer_child", side_effect=writer),
        patch(__name__ + ".external_image", side_effect=lambda *_args: FakeImage(_args[-1])),
    ]
    from contextlib import ExitStack
    with ExitStack() as stack:
        for item in patches:
            stack.enter_context(item)
        passed = outer_half("counted")
        if not passed.get("qualified") or calls != {"stage": 1, "cases": len(CASES), "writer": 2,
                                                       "attach": 1, "detach": 1}:
            raise ValueError(f"the real entry composition did not pass: {passed}, {calls}")
        options["group_closed"] = False
        if outer_half("counted").get("qualified"):
            raise ValueError("a non-joined group census was accepted")
        options["group_closed"] = True
        options["image_closed"] = False
        if outer_half("counted").get("qualified"):
            raise ValueError("an unclosed image was accepted")
        options["image_closed"] = True
        options["attach_ok"] = False
        before_detach = calls["detach"]
        uncertain = outer_half("counted")
        if uncertain.get("qualified") or calls["detach"] != before_detach:
            raise ValueError("an uncertain attachment was detached or accepted")
        options["attach_ok"] = True
        options["case_ok"] = False
        if outer_half("counted").get("qualified"):
            raise ValueError("a foreign case verdict was accepted")
        options["case_ok"] = True
        options["stage_ok"] = False
        before_cases = calls["cases"]
        if outer_half("counted").get("qualified") or calls["cases"] != before_cases:
            raise ValueError("a missing build artifact launched a case")
        with patch.object(journal, "external_supervisor_digest", return_value="0" * 64):
            if outer_half("counted").get("qualified"):
                raise ValueError("the wrong supervisor was accepted")
        options["stage_ok"] = True
        with patch.object(journal, "external_write_file", side_effect=OSError("injected report failure")):
            if outer_half("counted").get("qualified"):
                raise ValueError("a failed report publication was accepted")
        with patch.object(journal.base, "scratch_route_facts", return_value={"volume_uuid": "wrong"}):
            direct = dict(guard_env, **{base.WALL_DEADLINE_ENV: str(time.monotonic_ns() + 600_000_000_000)})
            with patch.object(journal, "external_environment", return_value=direct):
                refused = inner_half("counted", InnerEffects.from_environment())
            if refused.get("qualified_inner") or "receipt_path" in refused:
                raise ValueError("an unverified guard path wrote a report")

        # Exercise the real writer receiver and its path check, with only the
        # direct-child process and readiness wait replaced. The opened file is
        # this control's own private fixture, not a host or image observation.
        import io
        payload = os.path.join(fixture, "writer-payload")
        with open(payload, "wb") as handle:
            handle.write(b"fixture")

        class FakeChild:
            def __init__(self, role, path, consume=True):
                self.role = role
                self.consume = consume
                self.pid = 4_242 if role == "holder" else 4_243
                if role == "holder":
                    facts = os.stat(payload)
                    message = {"stage": "holder-ready", "pid": self.pid, "start": 9, "uid": os.getuid(),
                               "fd": 7, "device": facts.st_dev, "inode": facts.st_ino}
                else:
                    message = {"case": "unknown-writer", "stage": "writer-ready", "payload": path}
                self.stdout = io.StringIO(json.dumps(message) + "\n")
                self.returncode = 0
                self.killed = False
                self.reaped = False

            def communicate(self, data=None, timeout=None):
                self.reaped = True
                if self.role == "holder":
                    if data is not None and data != "release\n":
                        raise ValueError("wrong holder release")
                    return json.dumps({"case": "__hold", "status": "passed"}), ""
                if data is None:
                    return "", ""
                identity = json.loads(data)
                injection = {**identity, "case": "unknown-writer", "stage": "injection",
                             "reader": "descriptor-path-eio", "consumed": self.consume}
                return json.dumps(injection) + "\n" + json.dumps({"case": "unknown-writer", "status": "passed"}), ""

            def poll(self):
                return None if not self.reaped else self.returncode

            def kill(self):
                self.killed = True

        child, holder = FakeChild("driver", payload), FakeChild("holder", payload)
        with patch.object(subprocess, "Popen", side_effect=[child, holder]), patch.object(select, "select", side_effect=lambda files, *_args: (files, [], [])):
            received = actual_writer(fake_binary, "unknown-writer", {}, 5, fixture)
        if verdict(received, "unknown-writer").get("status") != "passed" or not child.reaped or not holder.reaped:
            raise ValueError("the actual writer receiver did not complete its handshake")
        child, holder = FakeChild("driver", payload, consume=False), FakeChild("holder", payload)
        with patch.object(subprocess, "Popen", side_effect=[child, holder]), patch.object(select, "select", side_effect=lambda files, *_args: (files, [], [])):
            try:
                actual_writer(fake_binary, "unknown-writer", {}, 5, fixture)
            except ValueError:
                pass
            else:
                raise ValueError("the receiver credited an unconsumed injection")
        if not child.reaped or not holder.reaped:
            raise ValueError("an unconsumed injection left a direct child unreaped")
        child = FakeChild("driver", os.path.join(os.path.dirname(fixture), "outside"))
        with patch.object(subprocess, "Popen", return_value=child), patch.object(select, "select", return_value=([child.stdout], [], [])):
            try:
                actual_writer(fake_binary, "unknown-writer", {}, 5, fixture)
            except ValueError:
                pass
            else:
                raise ValueError("the writer receiver accepted an escaping path")
        if not child.killed or not child.reaped:
            raise ValueError("the refused direct child was not killed and reaped")
        empty = {"certain": True, "members": 0, "uncertain": 0}
        base_receipt = {"outcome": {"closure": "closed", "launched": True, "fault": None,
                                    "deadline": False, "status": 0}, "census_join":
                        {"joined": True, "deciding": empty, "final": empty}}
        if not group_closed(base_receipt):
            raise ValueError("the positive joined group census was refused")
        for field in ("deciding", "final"):
            altered = {**base_receipt, "census_join": {**base_receipt["census_join"], field: None}}
            if group_closed(altered):
                raise ValueError(f"a missing {field} census was accepted")
    return {"fixture": fixture, "controls": "entry, stage, image, case, report and group-closure composition"}


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic"])
    parser.add_argument("--inner", choices=["counted", "diagnostic"])
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args(argv)
    if args.self_check:
        try:
            print(json.dumps(composition_controls()))
            return 0
        except Exception as error:
            print(f"coordinator proof controls failed: {error}", file=sys.stderr)
            return 1
    if args.plan:
        print(json.dumps({"schema": SCHEMA, "cases": CASES, "bounds": {"seconds": LIMIT_SECONDS,
            "scratch_mib": SCRATCH_MIB, "image_mib": IMAGE_MIB, "work_margin_seconds": WORK_MARGIN},
            "execution_ready": EXECUTION_READY, "reason": "native sessions require separate owner activation"}))
        return 0
    if not EXECUTION_READY:
        print(json.dumps({"schema": SCHEMA, "qualified": False,
                          "refused": "the source apparatus is not ready for native execution"}))
        return 125
    if args.inner:
        result = inner_half(args.inner, InnerEffects.from_environment())
        print(json.dumps(result))
        return 0 if result.get("qualified_inner") else 1
    if not args.mode:
        parser.error("choose --plan or a separately activated mode")
    try:
        result = outer_half(args.mode)
    except Exception as error:
        result = {"schema": SCHEMA, "stage": "outer", "mode": args.mode,
                  "qualified": False, "fault": str(error)[:500]}
    print(json.dumps(result))
    return 0 if result.get("qualified") else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
