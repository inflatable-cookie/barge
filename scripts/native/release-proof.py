#!/usr/bin/env python3
"""Barge F6-R1's native proof: the release effect on one owned synthetic APFS image.

  python3 scripts/native/release-proof.py stage         # build and stage the proof binary, effect-free
  python3 scripts/native/release-proof.py counted       # the author pass (owner-activated)
  python3 scripts/native/release-proof.py diagnostic    # the uncounted twin (owner-activated)
  python3 scripts/native/release-proof.py --self-check  # apparatus only, effect-free
  python3 scripts/native/release-proof.py --plan        # the plan, effect-free

It is owner-activated. No ordinary Effigy selector runs a counted or diagnostic pass,
and the declarations activate nothing. The binary under test is never named by the
caller: each counted or diagnostic run first stages a proof-feature build of HEAD
through Oxide, hashes it, and runs only that staged file.

What it does, in one sentence: under Oxide's qualified shared supervisor (pinned
below, executed with the same interpreter), it creates one private run root inside
the guard's owned scratch, attaches one owned APFS sparse image of 64 MiB with
ownership enabled, registers that image as the only volume, and runs twelve release
cases through the staged `barge` CLI against a private HOME, a private state
directory and a synthetic account home (BARGE_ACCOUNT_HOME, equal to HOME). Each case
declares its own area, so one case's roots never reach another case's plan.

The staged binary is the `proof-synthetic-account` feature build. Under that feature
alone, Oxide's reference directories come from the synthetic account home. The
ordinary production build is a different binary and this proof does not run it. The
receipt records that difference as a limitation (LIMITATION below).

What it never does:

  - it never touches the real account home or the real Barge state directory, and it
    never registers a real drive (Builds included);
  - it never reads or writes Oxide's real account control: the staged build resolves
    Oxide's directories from the synthetic home only, and every case records the
    reference control it chose, which must lie inside the owned scratch;
  - it never deletes anything but synthetic roots inside its own image;
  - it never force-detaches; detach is the ordinary `hdiutil detach`;
  - it never writes outside the guard's owned scratch and its evidence directory;
  - it never runs without the supervisor's contract environment.

Modes:

  counted     Every case must pass. The first fault stops the remaining cases, a case
              whose dependency did not pass is skipped, and the receipt is
              `qualified: true` only on a clean pass.
  diagnostic  The uncounted twin: keep going after a fault, record every case, and
              always write `qualified: false`. It proves nothing on its own.

Receipt: $OXIDE_PROOF_EVIDENCE/release-proof-receipt.json. A missing receipt is a
missing proof.
"""

import argparse
import ast
import contextlib
import hashlib
import io
import json
import os
import plistlib
import pwd
import re
import shutil
import subprocess
import sys
import tempfile
import time
import traceback
from unittest import mock

SCHEMA = "barge.release.native/1"
SELF_CHECK_SCHEMA = "barge.release.native.self-check/1"
SCRIPT_PATH = os.path.abspath(__file__)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT_PATH)))
SUPERVISOR_NAME = "scripts/bounded-test.py"
DEFAULT_SUPERVISOR = os.path.expanduser("~/Dev/projects/oxide/scripts/bounded-test.py")
# The qualified supervisor's exact bytes, the same pin the S6, S7 and space proofs
# name. Barge writes no second supervisor; the pin moves only with a brief that names
# the head it verified.
SUPERVISOR_SHA256 = "fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b"
DESIGNATED_SCRATCH = "/Volumes/Builds/scratch/barge"
LIMIT_SECONDS = 300
SCRATCH_BUDGET_MIB = 512
# One small owned image. Sparse, so the allocated size is far below the logical size.
IMAGE_MIB = 64
# `hdiutil create` does not take `-owners`; ownership is enabled at attach.
CREATE_IMAGE_OPTIONS = [
    "-size", f"{IMAGE_MIB}m", "-fs", "APFS", "-type", "SPARSE",
    "-volname", "barge-proof-release",
]
ATTACH_OPTIONS = ["-nobrowse", "-owners", "on"]
REAL_STATE_RELATIVE = "Library/Application Support/barge/state"
REAL_STATE_ENTRIES = 4096
REAL_STATE_BYTES = 8 * 1024 * 1024
REAL_STATE_SECONDS = 20
# The area's declared prune age, and how far a fixture root is backdated.
AREA_AGE_SECONDS = 60
AGED_DAYS = 30
EXIT_INFRASTRUCTURE = 4
OXIDE_CHECKOUT_ID = "c" * 64
OXIDE_RELATIVE_REGISTRY = os.path.join(
    "Library", "Caches", "oxide", "build-pool", "oxide-managed", "outputs", "registry.json"
)
OXIDE_REGISTRY_VERSION = "oxide.outputs.registry/2"
VOLUME_UUID_KEY = "VolumeUUID"
HOLDER = "release-native-holder"
PROOF_RECEIPT = "release-proof-receipt.json"
# Any holder or name that is not this proof's own is refused.
NAMESPACE = "example"
# The one Cargo feature the staged binary is built with. It is named here, in the
# build command, the staged manifest and the self-check's manifest reading.
PROOF_FEATURE = "proof-synthetic-account"
BUILD_COMMAND = [
    "oxide", "build", "-p", "barge-cli", "--locked",
    "--features", PROOF_FEATURE, "--message-format", "json",
]
STAGE_ROOT = os.path.join(DESIGNATED_SCRATCH, "release-proof-stage")
STAGE_MANIFEST = "proof-binary.json"
STAGE_SCHEMA = "barge.release.proof-binary/1"
# Oxide's account directories under the synthetic home, as the proof build resolves
# them: the same relative layout the ordinary build uses under the password home.
OXIDE_STATE_RELATIVE = os.path.join("Library", "Application Support", "oxide", "state")
OXIDE_CACHE_RELATIVE = os.path.join("Library", "Caches", "oxide")
LIMITATION = (
    "The end-to-end receipt runs a proof-only build (feature `proof-synthetic-account`) "
    "whose Oxide reference directories come from one synthetic account home. It tests "
    "the release logic over synthetic AccountFacts. It does not establish that the "
    "production account lookup is equivalent, and the production build is not run here."
)

ENVIRONMENT = {
    "TMPDIR": "the guard's owned scratch; every fixture directory is created here",
    "TMP": "the guard's owned scratch, kept equal to TMPDIR",
    "TEMP": "the guard's owned scratch, kept equal to TMPDIR",
    "BARGE_STATE_DIR": "the private fixture state directory this proof creates",
    "BARGE_ACCOUNT_HOME": "the synthetic account home, equal to HOME; Oxide's directories hang from it in the proof build",
    "BARGE_PROOF_SUPERVISOR": "the optional path to the hash-pinned Oxide supervisor",
    "OXIDE_PROOF_EVIDENCE": "the supervisor's evidence directory for the receipt",
    "OXIDE_TASK_SCRATCH_ROOT": "the supervisor's owned scratch root that TMPDIR must be inside",
    "DYLD_INSERT_LIBRARIES": "discarded from both halves: this proof injects no interposer",
    "HOME": "a private fixture home, so no real account state is read or written",
    "CARGO_HOME": "a private fixture Cargo home; this proof builds nothing",
}

# Each case is one release claim, carried by one named regression in the source tree
# and exercised here end to end. `carried_by` must name a real test; the self-check
# proves it. `method` must name a Proof method; the self-check proves that too.
CASES = [
    {"id": "R1", "method": "case_released_one_synthetic_root",
     "claim": "one aged, unreferenced synthetic root is released from the presented bytes; replaying those bytes releases nothing",
     "carried_by": "a_release_run_deletes_one_root_the_presented_plan_allows_and_replaying_it_releases_nothing"},
    {"id": "R2", "method": "case_missing_identity_record_keeps_the_root",
     "claim": "a missing durable identity record keeps the root; the run writes nothing",
     "carried_by": "all_six_conditions_release_a_root_and_each_one_alone_keeps_it"},
    {"id": "R3", "method": "case_live_hold_keeps_the_root",
     "claim": "a live hold keeps the root through custody",
     "carried_by": "a_run_keeps_a_root_whose_condition_changed_after_the_plan_was_written"},
    {"id": "R4", "method": "case_named_root_waives_only_the_age_gate",
     "claim": "a young root is kept; a named root is released with its waiver stated in the plan",
     "carried_by": "a_named_root_waives_only_the_age_gate_and_the_plan_says_so"},
    {"id": "R5", "method": "case_pending_oxide_reference_keeps_the_root",
     "claim": "a pending Oxide reference names the root as current and keeps it",
     "carried_by": "an_oxide_reference_that_is_not_settled_keeps_the_root"},
    {"id": "R6", "method": "case_release_by_reference_needs_the_consumer_path_absent",
     "claim": "a settled Oxide row releases only once its consumer path is positively absent",
     "carried_by": "a_settled_oxide_row_releases_only_with_an_absent_consumer_path"},
    {"id": "R7", "method": "case_incomplete_reference_set_refuses_the_batch",
     "claim": "an incomplete Oxide reference set refuses the whole batch and changes nothing",
     "carried_by": "an_incomplete_or_divergent_reference_source_keeps_the_whole_batch"},
    {"id": "R8", "method": "case_foreign_marker_keeps_the_root",
     "claim": "a marker whose kind is not one this gate knows cannot qualify its root",
     "carried_by": "a_marker_with_an_unknown_kind_cannot_qualify_its_root"},
    {"id": "R9", "method": "case_changed_digest_refuses_and_changes_nothing",
     "claim": "a digest that is not the presented bytes' digest refuses; the same bytes with their digest then release",
     "carried_by": "a_changed_digest_a_non_canonical_plan_or_another_area_refuses_and_changes_nothing"},
    {"id": "R10", "method": "case_reappeared_consumer_path_keeps_the_root",
     "claim": "a consumer path that reappears between plan and run keeps the root",
     "carried_by": "an_oxide_reference_that_changes_after_the_plan_keeps_the_root_or_the_batch"},
    {"id": "R11", "method": "case_intent_before_rename_then_remount_replay",
     "claim": "a pending intent is written before the rename; after an ordinary detach and reattach, a later run settles it by UUID",
     "carried_by": "an_intent_is_written_before_the_rename_and_a_remount_replays_it"},
    {"id": "R12", "method": "case_named_run_refuses_a_root_it_was_not_given",
     "claim": "a run releases only the rows its presented plan carries; a root added after the plan is never released by it",
     "carried_by": "a_release_run_deletes_one_root_the_presented_plan_allows_and_replaying_it_releases_nothing"},
]

# The byte-level oracle for the identity record: its path inside the state directory.
IDENTITY_RELATIVE = os.path.join("roots", "{root_id}", "identity.json")
# The Oxide fixture and the synthetic root both live in the run root only.
RUN_ROOT_PREFIX = "release-proof-"

# Spelled apart so this list does not contain the tokens it forbids.
FORBIDDEN_TOKENS = ["-" + "force", "p" + "kill", "kill" + "all"]


class Fault(Exception):
    """A case or premise did not hold."""

    def __init__(self, check, detail):
        super().__init__(f"{check}: {detail}")
        self.check = check
        self.detail = detail


class Infrastructure(Exception):
    """The host could not run the proof at all; not a verdict on the product."""

    def __init__(self, check, detail):
        super().__init__(f"{check}: {detail}")
        self.check = check
        self.detail = detail


def now_ms():
    return int(time.time() * 1000)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(command, env, cwd=None, timeout=120, check=True):
    result = subprocess.run(
        command, env=env, cwd=cwd, capture_output=True, text=True, timeout=timeout
    )
    if check and result.returncode != 0:
        raise Fault(
            "command",
            f"{' '.join(command)} exited {result.returncode}: {result.stderr[-400:]}",
        )
    return result


def account_home():
    return pwd.getpwuid(os.getuid()).pw_dir


def real_state_observation(deadline_seconds=REAL_STATE_SECONDS):
    """A bounded digest of the real account's Barge state, or why it is incomplete.

    It walks names, sizes, modes and modification times under the real state root,
    never contents. A walk that hits its entry, byte or time bound is `incomplete`,
    which the receipt reads as a failed premise rather than as an unchanged one.
    """
    root = os.path.join(account_home(), REAL_STATE_RELATIVE)
    if not os.path.lexists(root):
        return {"status": "absent", "digest": None, "reason": "no real state root"}
    deadline = time.monotonic() + deadline_seconds
    entries = []
    total = 0
    for directory, names, files in os.walk(root):
        if time.monotonic() > deadline:
            return {"status": "incomplete", "digest": None, "reason": "time bound"}
        for name in sorted(names + files):
            path = os.path.join(directory, name)
            try:
                info = os.lstat(path)
            except OSError as error:
                return {"status": "incomplete", "digest": None, "reason": str(error)}
            entries.append([os.path.relpath(path, root), info.st_size, info.st_mtime_ns, info.st_mode])
            total += info.st_size
            if len(entries) > REAL_STATE_ENTRIES or total > REAL_STATE_BYTES:
                return {"status": "incomplete", "digest": None, "reason": "entry or byte bound"}
    digest = hashlib.sha256(json.dumps(sorted(entries)).encode()).hexdigest()
    return {"status": "complete", "digest": digest, "reason": None}


def ownership_state(info_text):
    """The text form's explicit `Owners:` value, or None when it is not one."""
    values = []
    for line in info_text.splitlines():
        stripped = line.strip()
        if not stripped.lower().startswith("owners:"):
            continue
        _, _, value = stripped.partition(":")
        values.append(value.strip().lower())
    if len(values) != 1:
        return None
    return {"enabled": "enabled", "disabled": "disabled"}.get(values[0])


def supervisor_path():
    return os.environ.get("BARGE_PROOF_SUPERVISOR", DEFAULT_SUPERVISOR)


def installed_supervisor_problems():
    """Why the pin and the installed supervisor's bytes disagree. Spawns nothing."""
    path = supervisor_path()
    try:
        digest = sha256_file(path)
    except OSError as error:
        return [f"the pinned supervisor is unreadable: {error}"]
    if digest != SUPERVISOR_SHA256:
        return [f"the supervisor at {path} is sha256 {digest}, not the pinned {SUPERVISOR_SHA256}"]
    return []


def inner_command(mode, binary):
    """The argv the supervisor runs: this script, its mode, and the binary."""
    return [sys.executable, SCRIPT_PATH, "--inner", mode, "--binary", binary]


def supervisor_command(supervisor, mode, binary):
    return [
        sys.executable, supervisor,
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", DESIGNATED_SCRATCH,
        "--", *inner_command(mode, binary),
    ]


def argv_guard_problems():
    """Prove the outer command and the inner parser agree for both modes. Pure."""
    problems = []
    binary = "/nonexistent/barge-proof-binary"
    for mode in ("counted", "diagnostic"):
        command = supervisor_command("/nonexistent/bounded-test.py", mode, binary)
        if command[command.index("--") + 1:] != inner_command(mode, binary):
            problems.append(f"the {mode} command does not end in this script's inner argv")
        inner = inner_command(mode, binary)
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                parsed = argument_parser().parse_args(inner[2:])
            except SystemExit:
                problems.append(f"the {mode} inner argv does not parse with this script's parser")
                continue
        if parsed.inner != mode or parsed.binary != binary or parsed.mode is not None:
            problems.append(f"the {mode} inner argv parses to the wrong fields")
        broken = [sys.executable, SCRIPT_PATH, "--inner", mode, binary]
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                argument_parser().parse_args(broken[2:])
            except SystemExit:
                pass
            else:
                problems.append(f"an executable in the positional mode slot is accepted for {mode}")
    return problems


def case_table_problems():
    """Every case names a real Proof method and a real regression in the source tree."""
    problems = []
    source_names = set()
    for source in [
        os.path.join(REPO_ROOT, "crates", "barge", "src", "release.rs"),
        os.path.join(REPO_ROOT, "crates", "barge-cli", "tests", "cli_contract.rs"),
    ]:
        with open(source, encoding="utf-8") as handle:
            source_names.update(re.findall(r"fn ([a-z_0-9]+)\(", handle.read()))
    ids = [case["id"] for case in CASES]
    if len(ids) != len(set(ids)):
        problems.append("two cases share an id")
    for case in CASES:
        if not hasattr(Proof, case["method"]):
            problems.append(f"case {case['id']} names the missing method {case['method']}")
        if case["carried_by"] not in source_names:
            problems.append(f"case {case['id']} names the missing regression {case['carried_by']}")
    return problems


def source_problems():
    """The text of this script must not reach for a force detach or a process kill."""
    with open(SCRIPT_PATH, encoding="utf-8") as handle:
        text = handle.read()
    problems = []
    for token in FORBIDDEN_TOKENS:
        if token in text:
            problems.append(f"this script contains the forbidden token {token!r}")
    tree = ast.parse(text)
    if not any(isinstance(node, ast.ClassDef) and node.name == "Proof" for node in tree.body):
        problems.append("the Proof class is missing")
    return problems


def fixture_environment(home, state, root):
    """The variables this proof overrides, as a pure map. Pure: it touches nothing.

    HOME and BARGE_ACCOUNT_HOME name one synthetic account home, so Oxide's
    directories hang from it in the proof build. The state directory and the
    temporary directories sit under the run root.
    """
    return {
        "HOME": home,
        "CARGO_HOME": os.path.join(home, "cargo"),
        "BARGE_ACCOUNT_HOME": home,
        "BARGE_STATE_DIR": state,
        "TMPDIR": root,
        "TMP": root,
        "TEMP": root,
    }


def environment_problems(environment, scratch, home, state, root):
    """Why the variables do not bind the synthetic account home inside the scratch. Pure."""
    problems = []
    if environment.get("BARGE_ACCOUNT_HOME") != environment.get("HOME"):
        problems.append("BARGE_ACCOUNT_HOME is not the same path as HOME")
    if environment.get("BARGE_ACCOUNT_HOME") != home:
        problems.append("BARGE_ACCOUNT_HOME is not the synthetic home this run created")
    if environment.get("BARGE_STATE_DIR") != state:
        problems.append("BARGE_STATE_DIR is not the fixture state directory this run created")
    for name in ("TMPDIR", "TMP", "TEMP"):
        if environment.get(name) != root:
            problems.append(f"{name} is not the run root")
    problems += isolation_problems(scratch, {
        "run root": root, "synthetic home": home, "state directory": state,
    })
    return problems


def oxide_directories(home):
    """Oxide's state and cache directories under a synthetic home. Pure."""
    return {
        "state": os.path.join(home, OXIDE_STATE_RELATIVE),
        "cache": os.path.join(home, OXIDE_CACHE_RELATIVE),
    }


def isolation_problems(scratch, paths):
    """Each named path must resolve strictly inside the owned scratch. Resolves names only.

    A path that is absent still resolves by its literal parts, so the check holds
    before anything is created. A symlink that leaves the scratch is caught here.
    """
    problems = []
    owned = os.path.realpath(scratch)
    for name, path in paths.items():
        resolved = os.path.realpath(path)
        if resolved == owned or not resolved.startswith(owned + os.sep):
            problems.append(f"the {name} {path} resolves to {resolved}, outside the owned scratch {owned}")
    return problems


def executable_from_build(text):
    """The staged binary's source and features from `oxide build --message-format json`. Pure.

    Cargo reports one `compiler-artifact` per target. The `barge` binary's artifact
    names its executable and the features it was built with, so the staged bytes
    are checked against what cargo says it built, not against a guess.
    """
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            document = json.loads(line)
        except ValueError:
            continue
        target = document.get("target") or {}
        if document.get("reason") == "compiler-artifact" and target.get("name") == "barge" \
                and "bin" in target.get("kind", []) and document.get("executable"):
            return {"executable": document["executable"], "features": document.get("features", [])}
    return None


def staged_build_problems(binary, head):
    """Why a binary is not the staged proof build of this head. Reads the manifest and the bytes."""
    manifest_path = os.path.join(os.path.dirname(os.path.abspath(binary)), STAGE_MANIFEST)
    try:
        with open(manifest_path, encoding="utf-8") as handle:
            manifest = json.load(handle)
    except (OSError, ValueError) as error:
        return [f"the staged manifest {manifest_path} is unreadable: {error}"]
    problems = []
    if manifest.get("schema") != STAGE_SCHEMA:
        problems.append("the staged manifest is not this proof's schema")
    if manifest.get("path") != os.path.abspath(binary):
        problems.append("the binary is not the path the staged manifest names")
    if manifest.get("features") != [PROOF_FEATURE]:
        problems.append(f"the staged build's features are {manifest.get('features')}, not [{PROOF_FEATURE!r}]")
    if manifest.get("head") != head:
        problems.append(f"the staged build is for head {manifest.get('head')}, not {head}")
    try:
        digest = sha256_file(binary)
    except OSError as error:
        return problems + [f"the staged binary is unreadable: {error}"]
    if manifest.get("sha256") != digest:
        problems.append("the staged binary's bytes no longer match the staged manifest's digest")
    return problems


def stage_proof_binary():
    """Build the proof-feature binary of HEAD through Oxide and stage a hashed copy.

    The copy sits in its own stage directory under the designated scratch. The
    ordinary production build (target/debug/barge without the feature) is never
    the one staged. Raises Infrastructure when the build or the copy cannot be made.
    """
    head = git_head()
    if not head:
        raise Infrastructure("stage", "HEAD is unreadable, so the staged build cannot be named")
    stage_env = dict(os.environ)
    stage_env.pop("RUSTFLAGS", None)
    stage_env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    stage_env["RUSTUP_AUTO_INSTALL"] = "0"
    build = subprocess.run(BUILD_COMMAND, cwd=REPO_ROOT, env=stage_env,
                           capture_output=True, text=True, timeout=1800)
    if build.returncode != 0:
        raise Infrastructure("stage", f"oxide build exited {build.returncode}: {build.stderr[-400:]}")
    found = executable_from_build(build.stdout)
    if found is None:
        raise Infrastructure("stage", "the build reported no barge binary artifact")
    if found["features"] != [PROOF_FEATURE]:
        raise Infrastructure("stage", f"the barge artifact was built with features {found['features']}, not [{PROOF_FEATURE!r}]")
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    directory = os.path.join(STAGE_ROOT, f"{head}-{stamp}-{os.getpid()}")
    os.makedirs(directory)
    staged = os.path.join(directory, "barge")
    shutil.copyfile(found["executable"], staged)
    os.chmod(staged, 0o755)
    manifest = {
        "schema": STAGE_SCHEMA,
        "head": head,
        "features": found["features"],
        "build_command": BUILD_COMMAND,
        "source": found["executable"],
        "path": staged,
        "sha256": sha256_file(staged),
    }
    with open(os.path.join(directory, STAGE_MANIFEST), "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, indent=1)
        handle.write("\n")
    return manifest


def rebound_path(old_mount, new_mount, old_path, identity, uuid):
    """Bind only the recorded relative identity to the independently verified UUID."""
    relative = os.path.relpath(old_path, old_mount)
    if (old_mount == new_mount or not os.path.isabs(old_path)
            or relative in (".", "..") or relative.startswith(".." + os.sep)
            or os.path.normpath(old_path) != old_path
            or identity.get("volume_uuid") != uuid
            or identity.get("mount_relative_path") != relative):
        raise Fault("R11", "root path, relative identity or remounted UUID does not match")
    return os.path.join(new_mount, relative)


def verify_rollback(path, identity, identity_bytes, marker_bytes, payload, pending):
    if pending:
        raise Fault("R11", "replay left an intent pending")
    try:
        with open(identity["record_path"], "rb") as handle:
            same_record = handle.read() == identity_bytes
        with open(os.path.join(path, ".barge-root.json"), "rb") as handle:
            same_marker = handle.read() == marker_bytes
        with open(os.path.join(path, "proof-content"), "rb") as handle:
            same_content = handle.read() == payload
        same_inode = os.stat(path, follow_symlinks=False).st_ino == identity["inode"]
    except OSError as error:
        raise Fault("R11", f"rollback root cannot be observed: {error}") from error
    if not (same_record and same_marker and same_content and same_inode):
        raise Fault("R11", "rollback identity or content changed")


class Proof:
    """One counted or diagnostic run: one image, one private HOME and state, many areas."""

    def __init__(self, binary, receipt_path, diagnostic, build):
        self.binary = binary
        self.receipt_path = receipt_path
        self.diagnostic = diagnostic
        self.build = build
        self.isolation = None
        self.references = []
        self.root = None
        self.home = None
        self.state = None
        self.image = None
        self.mount = None
        self.mount_history = []
        self.uuid = None
        self.device = None
        self.device_after_remount = None
        self.env = None
        self.attached = False
        self.results = []
        self.first_fault = None
        self.preflight = None
        self.infrastructure = None
        self.unknown_custody = False
        self.real_state = None
        self.ownership = None
        self.closure = None
        self.started = time.time()

    # -- setup ------------------------------------------------------------

    def setup(self):
        scratch = os.environ.get("TMPDIR")
        if not scratch or not os.path.isdir(scratch):
            raise Fault("premise", "TMPDIR is not a directory the guard created")
        self.real_state = {"before": real_state_observation(), "after": None, "unchanged": None}
        self.root = tempfile.mkdtemp(prefix=RUN_ROOT_PREFIX, dir=scratch)
        self.home = tempfile.mkdtemp(prefix="home-", dir=self.root)
        self.state = tempfile.mkdtemp(prefix="state-", dir=self.root)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("OXIDE_", "XDG_", "BARGE_"))
                    and key not in ("DYLD_INSERT_LIBRARIES", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
        self.env["OXIDE_TASK_SCRATCH_ROOT"] = os.environ["OXIDE_TASK_SCRATCH_ROOT"]
        self.env["OXIDE_PROOF_EVIDENCE"] = os.environ["OXIDE_PROOF_EVIDENCE"]
        self.env["OXIDE_TEST_HOME_FROM_ENV"] = "1"
        self.env["RUSTUP_AUTO_INSTALL"] = "0"
        self.env.update(fixture_environment(self.home, self.state, self.root))
        os.makedirs(self.env["CARGO_HOME"], exist_ok=True)
        owned = os.environ["OXIDE_TASK_SCRATCH_ROOT"]
        oxide = oxide_directories(self.home)
        problems = environment_problems(self.env, owned, self.home, self.state, self.root)
        self.isolation = {
            "scratch": os.path.realpath(owned),
            "run_root": os.path.realpath(self.root),
            "synthetic_home": os.path.realpath(self.home),
            "state_directory": os.path.realpath(self.state),
            "oxide_state_directory": os.path.realpath(oxide["state"]),
            "oxide_cache_directory": os.path.realpath(oxide["cache"]),
            "problems": problems,
        }
        if problems:
            raise Fault("premise", "; ".join(problems))
        for name in ("hdiutil", "diskutil"):
            if shutil.which(name) is None:
                raise Infrastructure("host", f"{name} is not available")
        self.image = os.path.join(self.root, "release-volume.sparseimage")
        run(["hdiutil", "create", *CREATE_IMAGE_OPTIONS, self.image], self.env)
        self.attach(first=True)
        self.write_registry([self.volume_record()])
        code, _ = self.barge("describe", expect=0)
        if code != 0:
            raise Fault("premise", "the binary under test does not run `describe`")

    def attach(self, first):
        name = f"barge-release-proof-{os.getpid()}-{int(time.time()) % 100000}-{'a' if first else 'b'}"
        mount = f"/Volumes/{name}"
        self.mount = mount
        self.attached = True  # An interrupted attach has unknown custody, not absence.
        run(["hdiutil", "attach", self.image, "-mountpoint", mount, *ATTACH_OPTIONS], self.env)
        self.mount_history.append(mount)
        info = plistlib.loads(run(["diskutil", "info", "-plist", mount], self.env).stdout.encode())
        uuid = info.get(VOLUME_UUID_KEY)
        device = info.get("DeviceIdentifier")
        if not uuid or not device:
            raise Fault("premise", "the owned image did not report its UUID and device")
        text = run(["diskutil", "info", mount], self.env).stdout
        state = ownership_state(text)
        if first:
            self.uuid = uuid
            self.device = device
            self.ownership = {"attach_argv": list(ATTACH_OPTIONS), "text_state": state}
        else:
            if uuid != self.uuid:
                raise Fault("premise", "the reattached image reports a different volume UUID")
            self.device_after_remount = device
        if state != "enabled":
            raise Fault("premise", f"the image does not state `Owners: Enabled` (reads {state!r})")

    def detach(self):
        """Ordinary detach only. Returns whether the mount is gone afterwards."""
        result = run(["hdiutil", "detach", self.mount], self.env, check=False, timeout=120)
        gone = not os.path.lexists(self.mount)
        if result.returncode == 0 or gone:
            self.attached = False
        return {"exit": result.returncode, "mount_absent": gone, "mount": self.mount}

    def volume_record(self):
        return {
            "volume_uuid": self.uuid,
            "mount_hint": self.mount,
            "availability": "permanent",
            "durability": "expendable",
            "root_dir": "",
            "folders": {"scratch": None, "caches": None},
            "origin": "declared",
            "scratch_grants": {"delegations": [], "claims": []},
            "generation": 1,
            "registered_ms": now_ms(),
        }

    def write_registry(self, volumes):
        with open(os.path.join(self.state, "registry.json"), "w", encoding="utf-8") as handle:
            json.dump({"schema": "barge.registry/1", "volumes": volumes}, handle, indent=1)
            handle.write("\n")

    # -- the CLI ----------------------------------------------------------

    def barge(self, *args, expect=None, timeout=120):
        """Run the binary with `--json`; return (exit, document). `expect` is checked."""
        command = [self.binary, *args, "--json"]
        result = run(command, self.env, cwd=self.root, timeout=timeout, check=False)
        try:
            document = json.loads(result.stdout)
        except ValueError:
            raise Fault("barge", f"{' '.join(args)} printed no JSON (exit {result.returncode})")
        if expect is not None and result.returncode != expect:
            raise Fault(
                "barge",
                f"{' '.join(args)} exited {result.returncode}, expected {expect}: "
                f"{json.dumps(document)[:300]}",
            )
        return result.returncode, document

    def declare(self, area, prune):
        self.barge(
            "area", "declare", "--namespace", NAMESPACE, "--area", area,
            "--folder", area, "--absent", "wait", "--prune", prune,
            "--reserve", "excluded", expect=0,
        )

    def new_root(self, area, aged):
        _code, document = self.barge(
            "area", "root", "create", "--namespace", NAMESPACE, "--area", area,
            "--volume", self.uuid, "--kind", "area_root", expect=0,
        )
        root = document["data"]["root"]
        if aged:
            self.age_root(root["path"])
        return root

    def preview(self, area, plan_file, named=(), expect=0):
        args = ["prune", "--plan", "--area", f"{NAMESPACE}/{area}", "--volume", self.uuid,
                "--plan-out", plan_file]
        for root_id in named:
            args += ["--root", root_id]
        code, document = self.barge(*args, expect=expect)
        self.record_reference(os.path.basename(plan_file), document)
        return code, document

    def record_reference(self, label, document):
        """Record the reference control this preview chose; it must lie in the owned scratch."""
        oxide = (document.get("data") or {}).get("oxide") or {}
        control = oxide.get("control")
        self.references.append({"plan": label, "complete": oxide.get("complete"), "control": control})
        if control is not None:
            problems = isolation_problems(self.root, {"reference control": control})
            if problems:
                raise Fault("isolation", "; ".join(problems))

    def real(self, plan_file, digest, expect=0):
        return self.barge(
            "prune", "--plan", plan_file, "--plan-sha256", digest,
            "--area", f"{NAMESPACE}/{self.current_area}", "--volume", self.uuid,
            expect=expect,
        )

    def refused(self, plan_file, digest, area):
        """A real run that must refuse: returns the error kind and exit, changes nothing."""
        code, document = self.barge(
            "prune", "--plan", plan_file, "--plan-sha256", digest,
            "--area", f"{NAMESPACE}/{area}", "--volume", self.uuid,
        )
        if code == 0:
            raise Fault("refusal", "a run that must refuse exited 0")
        return {"exit": code, "kind": document.get("error", {}).get("kind")}

    # -- fixtures ---------------------------------------------------------

    def plan_path(self, case):
        return os.path.join(self.root, f"{case}-plan.json")

    def rows(self, document):
        return document["data"]["plan"]["rows"]["rows"]

    def row_for(self, document, root):
        matches = [row for row in self.rows(document) if row["root_id"] == root["root_id"]]
        if len(matches) != 1:
            raise Fault("plan", f"the plan names {len(matches)} rows for the fixture root")
        return matches[0]

    def age_root(self, root_path):
        """Backdate an owned root: its marker first, then its whole tree (S7's order)."""
        stamp = now_ms() - AGED_DAYS * 86400 * 1000
        marker = os.path.join(root_path, ".barge-root.json")
        info = os.stat(marker)
        with open(marker, encoding="utf-8") as handle:
            document = json.load(handle)
        document["created_ms"] = stamp
        with open(marker, "w", encoding="utf-8") as handle:
            json.dump(document, handle, indent=2)
            handle.write("\n")
        os.chmod(marker, info.st_mode & 0o777)
        when = time.strftime("%Y%m%d%H%M", time.gmtime(time.time() - AGED_DAYS * 86400))
        run(["find", root_path, "-exec", "touch", "-h", "-t", f"{when}.00", "{}", "+"], self.env)

    def edit_marker_kind(self, root_path, kind):
        marker = os.path.join(root_path, ".barge-root.json")
        with open(marker, encoding="utf-8") as handle:
            document = json.load(handle)
        document["kind"] = kind
        with open(marker, "w", encoding="utf-8") as handle:
            json.dump(document, handle, indent=2)
            handle.write("\n")

    def identity_path(self, root):
        return os.path.join(self.state, IDENTITY_RELATIVE.format(root_id=root["root_id"]))

    def hold(self, root):
        self.barge(
            "area", "hold", "--root", root["root_id"], "--holder", HOLDER,
            "--pid", str(os.getpid()), "--scope", "read",
            "--purpose", "the F6-R1 native proof's between-plan-and-run hold",
            expect=0,
        )

    def area_folder(self, area):
        return os.path.join(self.mount, area)

    def oxide_registry(self, rows, complete=True):
        """Publish one Oxide output-registry document under the private HOME."""
        path = os.path.join(self.home, OXIDE_RELATIVE_REGISTRY)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        document = {
            "version": OXIDE_REGISTRY_VERSION,
            "schema": 2,
            "revision": 1,
            "complete": complete,
            "created_by": "oxide",
            "rows": rows,
        }
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(document, handle)

    def oxide_row(self, root, checkout, pending=False, selected=False, on_volume=False):
        """One registry row. `on_volume` names the synthetic volume the checkout lives on.

        Absence is proved against the volume a row names, so a consumer path on the
        volume must say so; a row that does not name one cannot prove absence there.
        """
        receipt = os.path.join(self.root, "receipts", os.path.basename(checkout) + ".json")
        row = {
            "checkout_id": OXIDE_CHECKOUT_ID,
            "checkout": checkout,
            "receipt": receipt,
            "member": "target",
            "root": root["path"],
            "root_kind": "external",
            "recipe": "target-debug-v1",
            "generation": 1,
            "selected": selected,
            "pending": pending,
        }
        if on_volume:
            row["checkout_volume_uuid"] = self.uuid
        return row

    def pending_intents(self):
        directory = os.path.join(self.state, "intents")
        if not os.path.isdir(directory):
            return []
        pending = []
        for name in sorted(os.listdir(directory)):
            with open(os.path.join(directory, name), encoding="utf-8") as handle:
                document = json.load(handle)
            if document["phase"] != "deleted":
                pending.append(document)
        return pending

    def settled_intents(self):
        directory = os.path.join(self.state, "intents")
        if not os.path.isdir(directory):
            return []
        settled = []
        for name in sorted(os.listdir(directory)):
            with open(os.path.join(directory, name), encoding="utf-8") as handle:
                document = json.load(handle)
            if document["phase"] == "deleted":
                settled.append(document)
        return settled

    def plan_and_run(self, case, area, aged_root, named=(), expect_eligible=True):
        """One preview of an area and, when eligible, its real run. Returns evidence."""
        self.current_area = area
        plan_file = self.plan_path(case)
        _code, preview = self.preview(area, plan_file, named=named, expect=0)
        row = self.row_for(preview, aged_root)
        if row["eligible"] != expect_eligible:
            raise Fault(case, f"expected eligible={expect_eligible}, got {json.dumps(row)[:400]}")
        return preview, row, plan_file

    # -- the cases --------------------------------------------------------

    def fresh_area(self, name, prune):
        self.oxide_registry([], complete=True)
        self.declare(name, prune)
        return name

    def case_released_one_synthetic_root(self):
        area = self.fresh_area("c01", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        preview, row, plan_file = self.plan_and_run("R1", area, root)
        digest = preview["data"]["plan_digest"]
        _code, run_doc = self.real(plan_file, digest, expect=0)
        if run_doc["data"]["released_count"] != 1:
            raise Fault("R1", f"released {run_doc['data']['released_count']}, expected 1")
        if os.path.lexists(root["path"]):
            raise Fault("R1", "the released root is still present")
        if self.pending_intents():
            raise Fault("R1", "an intent is left pending after a clean release")
        settled = self.settled_intents()
        if len(settled) != 1 or settled[0].get("identity") is None:
            raise Fault("R1", "the settled intent does not keep the identity evidence")
        _code, again = self.real(plan_file, digest, expect=0)
        if again["data"]["released_count"] != 0:
            raise Fault("R1", "replaying the same bytes released a root")
        return {"root": root["root_id"], "released": 1, "replayed": again["data"]["released_count"],
                "intent_identity_kept": True}

    def case_missing_identity_record_keeps_the_root(self):
        area = self.fresh_area("c02", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        os.remove(self.identity_path(root))
        preview, row, plan_file = self.plan_and_run("R2", area, root, expect_eligible=False)
        if row["blocked_by"] != "identity":
            raise Fault("R2", f"blocked by {row['blocked_by']}, expected identity")
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 0 or not os.path.lexists(root["path"]):
            raise Fault("R2", "a root without its identity record was released")
        return {"root": root["root_id"], "blocked_by": row["blocked_by"],
                "counterfactual_of": "R1", "flipped_condition": "identity"}

    def case_live_hold_keeps_the_root(self):
        area = self.fresh_area("c03", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        self.hold(root)
        preview, row, plan_file = self.plan_and_run("R3", area, root, expect_eligible=False)
        if row["blocked_by"] != "custody":
            raise Fault("R3", f"blocked by {row['blocked_by']}, expected custody")
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 0 or not os.path.lexists(root["path"]):
            raise Fault("R3", "a held root was released")
        return {"root": root["root_id"], "blocked_by": row["blocked_by"],
                "counterfactual_of": "R1", "flipped_condition": "custody"}

    def case_named_root_waives_only_the_age_gate(self):
        area = self.fresh_area("c04", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=False)
        _p, young, _f = self.plan_and_run("R4-young", area, root, expect_eligible=False)
        # The plan omits `age_waived` when it is false, so an absent key is an unwaived row.
        if young["blocked_by"] != "policy" or young.get("age_waived"):
            raise Fault("R4", "an unnamed young root was not kept by the age gate")
        preview, row, plan_file = self.plan_and_run(
            "R4-named", area, root, named=[root["root_id"]], expect_eligible=True
        )
        if not row["age_waived"] or preview["data"]["plan"]["named_roots"] != [root["root_id"]]:
            raise Fault("R4", "the named waiver is not stated in the plan")
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 1 or os.path.lexists(root["path"]):
            raise Fault("R4", "the named root was not released under its own waiver")
        return {"root": root["root_id"], "young_blocked_by": "policy", "named_released": 1,
                "counterfactual_of": "R1", "flipped_condition": "age"}

    def case_pending_oxide_reference_keeps_the_root(self):
        area = self.fresh_area("c05", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        checkout = os.path.join(self.home, "checkouts", "pending")
        self.oxide_registry([self.oxide_row(root, checkout, pending=True)], complete=True)
        preview, row, plan_file = self.plan_and_run("R5", area, root, expect_eligible=False)
        if row["blocked_by"] != "references":
            raise Fault("R5", f"blocked by {row['blocked_by']}, expected references")
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 0 or not os.path.lexists(root["path"]):
            raise Fault("R5", "a root Oxide still marks pending was released")
        return {"root": root["root_id"], "blocked_by": row["blocked_by"],
                "counterfactual_of": "R1", "flipped_condition": "references"}

    def case_release_by_reference_needs_the_consumer_path_absent(self):
        area = self.fresh_area("c06", "release-on-reference")
        root = self.new_root(area, aged=False)
        # A consumer path on the synthetic volume: its absence is proved under that volume's
        # mount, which the absence anchor finds by UUID. A path under /Volumes that no
        # registered volume owns is unknown, and a consumer path must not be one of those.
        checkout = os.path.join(self.mount, "checkouts", "settled")
        os.makedirs(checkout)
        self.oxide_registry([self.oxide_row(root, checkout, on_volume=True)], complete=True)
        preview, row, plan_file = self.plan_and_run("R6-present", area, root, expect_eligible=False)
        if row["blocked_by"] != "absence" or (row.get("absence") or {}).get("verdict") != "present":
            raise Fault("R6", "a present consumer path did not keep the root")
        os.rmdir(checkout)
        preview, row, plan_file = self.plan_and_run("R6-absent", area, root, expect_eligible=True)
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 1 or os.path.lexists(root["path"]):
            raise Fault("R6", "an absent consumer path did not release by reference")
        return {"root": root["root_id"], "present_blocked_by": "absence", "released": 1,
                "policy": "release_on_reference"}

    def case_incomplete_reference_set_refuses_the_batch(self):
        area = self.fresh_area("c07", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        self.oxide_registry([], complete=False)
        self.current_area = area
        plan_file = self.plan_path("R7")
        # An incomplete Oxide set is a plan that needs a human: exit 3, and no row released.
        _code, preview = self.preview(area, plan_file, expect=3)
        row = self.row_for(preview, root)
        if row["eligible"]:
            raise Fault("R7", "a row was eligible under an incomplete reference set")
        if preview["data"]["oxide"]["complete"]:
            raise Fault("R7", "an incomplete registry reads as complete")
        refusal = self.refused(plan_file, preview["data"]["plan_digest"], area)
        if refusal["kind"] != "unknown_state":
            raise Fault("R7", f"the batch was refused as {refusal}, expected unknown_state")
        if not os.path.lexists(root["path"]):
            raise Fault("R7", "a root was released under an incomplete batch")
        return {"root": root["root_id"], "refusal": refusal, "counterfactual_of": "R1",
                "flipped_condition": "reference set complete"}

    def case_foreign_marker_keeps_the_root(self):
        area = self.fresh_area("c08", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        self.edit_marker_kind(root["path"], "foreign-kind")
        preview, row, plan_file = self.plan_and_run("R8", area, root, expect_eligible=False)
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 0 or not os.path.lexists(root["path"]):
            raise Fault("R8", "a foreign marker qualified its root")
        return {"root": root["root_id"], "blocked_by": row["blocked_by"],
                "counterfactual_of": "R1", "flipped_condition": "marker kind"}

    def case_changed_digest_refuses_and_changes_nothing(self):
        area = self.fresh_area("c09", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        preview, row, plan_file = self.plan_and_run("R9", area, root, expect_eligible=True)
        refusal = self.refused(plan_file, "0" * 64, area)
        if refusal["kind"] != "usage" or not os.path.lexists(root["path"]):
            raise Fault("R9", f"a changed digest did not refuse cleanly: {refusal}")
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 1:
            raise Fault("R9", "the presented bytes did not release after the refusal")
        return {"refusal": refusal, "then_released": 1}

    def case_reappeared_consumer_path_keeps_the_root(self):
        area = self.fresh_area("c10", "release-on-reference")
        root = self.new_root(area, aged=False)
        checkout = os.path.join(self.mount, "checkouts", "reappears")
        self.oxide_registry([self.oxide_row(root, checkout, on_volume=True)], complete=True)
        preview, row, plan_file = self.plan_and_run("R10", area, root, expect_eligible=True)
        os.makedirs(checkout)
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 0 or not os.path.lexists(root["path"]):
            raise Fault("R10", "a consumer path that reappeared did not keep its root")
        return {"root": root["root_id"], "released": 0, "kept_by": "absence"}

    def case_intent_before_rename_then_remount_replay(self):
        area = self.fresh_area("c11", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=False)
        payload = b"R11 original root content\n"
        with open(os.path.join(root["path"], "proof-content"), "wb") as handle:
            handle.write(payload)
        self.age_root(root["path"])
        identity_file = self.identity_path(root)
        with open(identity_file, "rb") as handle:
            identity_bytes = handle.read()
        identity = json.loads(identity_bytes)
        if identity["root_id"] != root["root_id"]:
            raise Fault("R11", "identity names another root")
        identity["record_path"] = identity_file
        with open(os.path.join(root["path"], ".barge-root.json"), "rb") as handle:
            marker_bytes = handle.read()
        old_mount = self.mount
        preview, row, plan_file = self.plan_and_run("R11", area, root, expect_eligible=True)
        digest = preview["data"]["plan_digest"]
        folder = self.area_folder(area)
        os.chmod(folder, 0o500)
        code, run_doc = self.real(plan_file, digest, expect=3)
        os.chmod(folder, 0o700)
        if run_doc["data"]["rows"][0]["outcome"] != "pending":
            raise Fault("R11", f"the rename refusal did not leave a pending row: {run_doc['data']['rows']}")
        pending = self.pending_intents()
        if len(pending) != 1 or pending[0].get("identity") is None:
            raise Fault("R11", "the pending intent does not carry the identity evidence")
        if not os.path.lexists(root["path"]):
            raise Fault("R11", "the unit moved before its rename succeeded")
        old_device = self.device
        detached = self.detach()
        if detached["exit"] != 0 or not detached["mount_absent"]:
            self.unknown_custody = True
            raise Infrastructure("detach", "ordinary detach did not establish a clean closure")
        self.attach(first=False)
        remounted = self.device_after_remount != old_device
        _code, replay = self.real(plan_file, digest, expect=0)
        if replay["data"]["replay_pending"] != 0 or self.pending_intents():
            raise Fault("R11", f"the remount replay left an intent pending: {replay['data']}")
        root = dict(root, path=rebound_path(old_mount, self.mount, root["path"], identity, self.uuid))
        verify_rollback(root["path"], identity, identity_bytes, marker_bytes, payload, self.pending_intents())
        preview2, row2, plan2 = self.plan_and_run("R11-after", area, root, expect_eligible=True)
        if row2["mount_relative_path"] != identity["mount_relative_path"] or row2["identity"]["verdict"] != "verified":
            raise Fault("R11", "fresh plan did not verify the original relative identity")
        _code, released = self.real(plan2, preview2["data"]["plan_digest"], expect=0)
        if (released["data"]["released_count"] != 1 or os.path.lexists(root["path"])
                or len(released["data"]["rows"]) != 1
                or released["data"]["rows"][0]["root_id"] != root["root_id"]):
            raise Fault("R11", "the root was not released after the replay settled")
        return {"root": root["root_id"], "pending_before_detach": 1, "detach": detached,
                "device_before": old_device, "device_after": self.device_after_remount,
                "device_changed": remounted, "replay_pending": 0, "released_after": 1}

    def reset_registration(self):
        """Use a new private state and a supported registration, only after R11 passes."""
        if not self.results or self.results[-1]["case"] != "R11" or self.results[-1]["status"] != "passed":
            raise Fault("registration", "R11 has not completed successfully")
        self.state = tempfile.mkdtemp(prefix="state-r12-", dir=self.root)
        self.env["BARGE_STATE_DIR"] = self.state
        _code, preview = self.barge(
            "volume", "register", "--mount", self.mount, "--availability", "permanent",
            "--durability", "expendable", "--root-dir", "", "--reserve", "1KiB",
            "--floor", "1KiB", "--declared-by", "release-proof", "--plan", expect=0)
        plan = preview["data"]["plan"]
        document = json.loads(plan)
        if document["record"]["volume_uuid"] != self.uuid or document["record"]["mount_hint"] != self.mount:
            raise Fault("registration", "registration plan did not bind the verified image")
        plan_file = os.path.join(self.root, "r12-register.json")
        with open(plan_file, "w", encoding="utf-8") as handle:
            handle.write(plan)
        self.barge("volume", "register", "--plan-file", plan_file,
                   "--plan-sha256", preview["data"]["plan_sha256"], expect=0)

    def case_named_run_refuses_a_root_it_was_not_given(self):
        self.reset_registration()
        area = self.fresh_area("c12", f"oldest-idle:{AREA_AGE_SECONDS}")
        given = self.new_root(area, aged=True)
        preview, row, plan_file = self.plan_and_run("R12", area, given, expect_eligible=True)
        digest = preview["data"]["plan_digest"]
        late = self.new_root(area, aged=True)
        _code, run_doc = self.real(plan_file, digest, expect=0)
        if run_doc["data"]["released_count"] != 1:
            raise Fault("R12", f"released {run_doc['data']['released_count']}, expected 1")
        if not os.path.lexists(late["path"]):
            raise Fault("R12", "a root the presented plan did not carry was released")
        return {"given": given["root_id"], "not_given_kept": late["root_id"], "released": 1}

    # -- the run ----------------------------------------------------------

    def run_cases(self):
        for case in CASES:
            name = case["id"]
            if name == "R12" and any(
                result["case"] == "R11" and result["status"] != "passed"
                for result in self.results
            ):
                self.results.append({"case": name, "status": "skipped",
                                     "detail": "R11 did not establish a known remount and registration state"})
                continue
            self.references = []
            try:
                evidence = getattr(self, case["method"])()
                self.results.append({"case": name, "status": "passed", "carried_by": case["carried_by"],
                                     "evidence": evidence, "reference_controls": self.references})
            except Infrastructure as error:
                self.unknown_custody = True
                self.infrastructure = {"check": error.check, "detail": error.detail}
                self.results.append({"case": name, "status": "failed", "kind": error.check,
                                     "detail": error.detail, "carried_by": case["carried_by"]})
                self.first_fault = self.first_fault or name
                self.stop_remaining(case, unknown=True)
                return
            except Fault as error:
                self.results.append({"case": name, "status": "failed", "kind": error.check,
                                     "detail": error.detail, "carried_by": case["carried_by"],
                                     "reference_controls": self.references})
                if self.first_fault is None:
                    self.first_fault = name
                if not self.diagnostic or error.check in ("command", "barge", "premise", "isolation"):
                    self.unknown_custody = error.check in ("command", "barge", "premise", "isolation")
                    self.stop_remaining(case, unknown=self.unknown_custody)
                    return
            except Exception as error:
                self.results.append({"case": name, "status": "failed", "kind": "exception",
                                     "detail": f"{type(error).__name__}: {error}",
                                     "trace": traceback.format_exc()[-800:],
                                     "carried_by": case["carried_by"],
                                     "reference_controls": self.references})
                if self.first_fault is None:
                    self.first_fault = name
                self.unknown_custody = True
                self.stop_remaining(case, unknown=True)
                return

    def stop_remaining(self, failed, unknown=False):
        index = CASES.index(failed)
        for case in CASES[index + 1:]:
            dependent = failed["id"] == "R11" and case["id"] == "R12"
            self.results.append({"case": case["id"],
                                 "status": "skipped" if dependent else "unexecuted",
                                 "detail": f"{'unknown outcome' if unknown else 'fail-fast'} at {failed['id']}"})

    def receipt_document(self, cancelled):
        passed = [result for result in self.results if result["status"] == "passed"]
        ran_all = len(passed) == len(CASES)
        return {
            "schema": SCHEMA,
            "mode": "diagnostic" if self.diagnostic else "counted",
            "diagnostic": self.diagnostic,
            "qualified": False,
            "classification": None,
            "head": git_head(),
            "binary": {"path": self.binary, "sha256": sha256_file(self.binary)},
            "build": self.build,
            "isolation": self.isolation,
            "limitation": LIMITATION,
            "supervisor": {"path": supervisor_path(), "sha256": SUPERVISOR_SHA256},
            "image": {"size_mib": IMAGE_MIB, "volume_uuid": self.uuid, "device": self.device,
                      "device_after_remount": self.device_after_remount,
                      "mounts": self.mount_history, "ownership": self.ownership},
            "resources": {"limit_seconds": LIMIT_SECONDS, "scratch_budget_mib": SCRATCH_BUDGET_MIB,
                          "wall_seconds": round(time.time() - self.started, 3)},
            "closure": self.closure,
            "preflight": self.preflight,
            "cases": self.results,
            "first_fault": self.first_fault,
            "cancelled": cancelled,
            "all_cases_passed": ran_all,
            "infrastructure": self.infrastructure,
        }

    def write_receipt(self, receipt):
        os.makedirs(os.path.dirname(self.receipt_path), exist_ok=True)
        with open(self.receipt_path, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=1)
            handle.write("\n")


def git_head():
    result = subprocess.run(["git", "-C", REPO_ROOT, "rev-parse", "HEAD"],
                            capture_output=True, text=True)
    return result.stdout.strip() or None


def inner_half(mode, binary, receipt_path):
    """The guarded run: the premises, the cases, the closure, then the receipt."""
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE")
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "")
    if not evidence or not os.path.isdir(evidence):
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "OXIDE_PROOF_EVIDENCE is not a directory; this is not the guard's run"}))
        return 125
    if not scratch or not os.path.isdir(scratch):
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "OXIDE_TASK_SCRATCH_ROOT is unset or absent; this is not the guard's run"}))
        return 125
    if not os.path.realpath(os.environ.get("TMPDIR", "")).startswith(os.path.realpath(scratch)):
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "TMPDIR is not inside the guard's owned scratch"}))
        return 125
    if receipt_path is None:
        receipt_path = os.path.join(evidence, PROOF_RECEIPT)
    head = git_head()
    build_problems = staged_build_problems(binary, head)
    if build_problems:
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "; ".join(build_problems)}))
        return 125
    with open(os.path.join(os.path.dirname(os.path.abspath(binary)), STAGE_MANIFEST), encoding="utf-8") as handle:
        build = json.load(handle)
    proof = Proof(binary, receipt_path, mode == "diagnostic", build)
    cancelled = None
    try:
        proof.setup()
        proof.preflight = {"problems": argv_guard_problems() + case_table_problems()}
        if proof.preflight["problems"]:
            raise Fault("preflight", "; ".join(proof.preflight["problems"]))
        proof.run_cases()
    except Infrastructure as error:
        proof.unknown_custody = True
        proof.infrastructure = {"check": error.check, "detail": error.detail}
    except Fault as error:
        proof.unknown_custody = proof.attached
        proof.results.append({"case": "setup", "status": "failed", "kind": error.check,
                              "detail": error.detail})
        proof.first_fault = proof.first_fault or "setup"
    except Exception as error:
        proof.unknown_custody = proof.attached
        proof.results.append({"case": "setup", "status": "failed", "kind": "exception",
                              "detail": f"{type(error).__name__}: {error}",
                              "trace": traceback.format_exc()[-800:]})
        proof.first_fault = proof.first_fault or "setup"
    finally:
        try:
            if proof.unknown_custody:
                proof.closure = {"exit": None, "mount_absent": False, "mount": proof.mount,
                                 "detail": "unknown custody; retained without repair"}
            elif proof.attached:
                proof.closure = proof.detach()
            else:
                proof.closure = {"exit": None, "mount_absent": True, "mount": proof.mount}
        except Exception as error:
            cancelled = f"the detach raised: {type(error).__name__}: {error}"
            proof.closure = {"exit": None, "mount_absent": False, "mount": proof.mount}

    recorded = {result["case"] for result in proof.results}
    for case in CASES:
        if case["id"] not in recorded:
            proof.results.append({"case": case["id"], "status": "unexecuted",
                                  "detail": "setup or infrastructure did not complete"})

    if proof.real_state is None:
        proof.real_state = {"before": {"status": "never observed", "digest": None}, "after": None}
    before = proof.real_state["before"]
    after = real_state_observation()
    proof.real_state["after"] = after
    premise_failure = None
    if before["status"] not in ("absent", "complete") or after["status"] not in ("absent", "complete"):
        premise_failure = (
            f"the real account state observation is incomplete: before {before['status']}, "
            f"after {after['status']}"
        )
    elif before["digest"] != after["digest"]:
        premise_failure = "the real account state changed during the run"
    proof.real_state["unchanged"] = premise_failure is None

    receipt = proof.receipt_document(cancelled)
    receipt["real_state"] = proof.real_state
    receipt["premise_failure"] = premise_failure
    qualified = (
        mode == "counted"
        and proof.infrastructure is None
        and receipt["all_cases_passed"]
        and proof.closure is not None
        and proof.closure["mount_absent"]
        and premise_failure is None
        and proof.isolation is not None
        and not proof.isolation["problems"]
    )
    receipt["qualified"] = bool(qualified)
    if proof.infrastructure is not None:
        receipt["classification"] = "infrastructure"
    elif qualified:
        receipt["classification"] = "qualified"
    else:
        receipt["classification"] = "unqualified"
    receipt["receipt_path"] = receipt_path
    proof.write_receipt(receipt)
    print(json.dumps({
        "schema": receipt["schema"],
        "qualified": receipt["qualified"],
        "classification": receipt["classification"],
        "all_cases_passed": receipt["all_cases_passed"],
        "first_fault": receipt["first_fault"],
        "premise_failure": premise_failure,
        "closure": receipt["closure"],
        "receipt_path": receipt_path,
        "cases": [{"case": result["case"], "status": result["status"]} for result in proof.results],
    }, indent=1))
    if receipt["qualified"]:
        return 0
    if proof.infrastructure is not None:
        return EXIT_INFRASTRUCTURE
    return 3 if mode == "diagnostic" else 1


def outer_half(mode):
    """Stage the proof build, verify the pinned supervisor, and hand this script to it."""
    supervisor = supervisor_path()
    if not os.path.isfile(supervisor) or not os.access(supervisor, os.R_OK):
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": f"the qualified supervisor is not a readable regular file at {supervisor}"}))
        return 125
    digest = sha256_file(supervisor)
    if digest != SUPERVISOR_SHA256:
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": f"{supervisor} is sha256 {digest}, not the qualified {SUPERVISOR_SHA256}"}))
        return 125
    if not os.path.isdir(DESIGNATED_SCRATCH):
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": f"the designated scratch parent is not present: {DESIGNATED_SCRATCH}"}))
        return 125
    try:
        manifest = stage_proof_binary()
    except Infrastructure as error:
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": f"staging the proof build failed: {error.detail}"}))
        return 125
    binary = manifest["path"]
    env = dict(os.environ)
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env["BARGE_PROOF_SUPERVISOR"] = supervisor
    result = subprocess.run(supervisor_command(supervisor, mode, binary), env=env)
    return result.returncode


def feature_problems():
    """The proof feature is declared, off by default, and the ordinary branch is pinned.

    Reads manifests and one source file. Spawns nothing and builds nothing.
    """
    problems = []
    barge_manifest = os.path.join(REPO_ROOT, "crates", "barge", "Cargo.toml")
    cli_manifest = os.path.join(REPO_ROOT, "crates", "barge-cli", "Cargo.toml")
    with open(barge_manifest, encoding="utf-8") as handle:
        barge_text = handle.read()
    with open(cli_manifest, encoding="utf-8") as handle:
        cli_text = handle.read()
    if f'{PROOF_FEATURE} = []' not in barge_text:
        problems.append(f"crates/barge does not declare the feature {PROOF_FEATURE}")
    if re.search(r"^default\s*=", barge_text, re.MULTILINE) or re.search(r"^default\s*=", cli_text, re.MULTILINE):
        problems.append("a default feature is declared, which would enable the proof build by default")
    if f'{PROOF_FEATURE} = ["barge/{PROOF_FEATURE}"]' not in cli_text:
        problems.append(f"crates/barge-cli does not forward the feature {PROOF_FEATURE} to barge")
    macos = os.path.join(REPO_ROOT, "crates", "barge", "src", "os", "macos.rs")
    with open(macos, encoding="utf-8") as handle:
        macos_text = handle.read()
    if f'#[cfg(not(feature = "{PROOF_FEATURE}"))]\nfn tool_account_home(_tool: &str)' not in macos_text:
        problems.append("macos.rs lacks the ordinary tool-home branch that ignores the environment")
    if not os.path.isfile(os.path.join(REPO_ROOT, "crates", "barge", "src", "os", "proof_account.rs")):
        problems.append("the proof-only account module is missing")
    return problems


def pure_self_check():
    """The pure helpers, pinned on canned inputs. Touches nothing outside the arguments."""
    problems = []
    scratch = os.path.join(DESIGNATED_SCRATCH, "self-check-owned")
    home = os.path.join(scratch, "home")
    state = os.path.join(scratch, "state")
    root = os.path.join(scratch, "run")
    environment = fixture_environment(home, state, root)
    if environment_problems(environment, scratch, home, state, root):
        problems.append("the fixture environment does not bind the synthetic home inside the scratch")
    if not environment_problems(dict(environment, BARGE_ACCOUNT_HOME="/Users/tom"), scratch, home, state, root):
        problems.append("a real account home bound as BARGE_ACCOUNT_HOME is not refused")
    if isolation_problems(scratch, {"escape": os.path.join(scratch, "..", "outside")}) == []:
        problems.append("a path that leaves the scratch by '..' is not refused")
    if isolation_problems(scratch, {"control": "/Users/tom/Dev/oxide/oxide-managed"}) == []:
        problems.append("the real Oxide control is not refused as outside the scratch")
    directories = oxide_directories(home)
    if not directories["state"].startswith(home + os.sep) or not directories["cache"].startswith(home + os.sep):
        problems.append("the Oxide directories do not hang from the synthetic home")
    canned = "\n".join([
        '{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"barge"},"executable":null,"features":["proof-synthetic-account"]}',
        "not json",
        '{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"barge"},"executable":"/x/target/debug/barge","features":["proof-synthetic-account"]}',
    ])
    found = executable_from_build(canned)
    if found != {"executable": "/x/target/debug/barge", "features": [PROOF_FEATURE]}:
        problems.append("the staged binary's artifact is not read from the build's JSON")
    if executable_from_build('{"reason":"build-finished"}') is not None:
        problems.append("a build with no barge binary artifact reports one")
    if not staged_build_problems(os.path.join(scratch, "absent", "barge"), "0" * 40):
        problems.append("a binary with no staged manifest is not refused")
    # Exercise the same path and rollback helpers used by R11, without a filesystem.
    identity = {"volume_uuid": "uuid", "mount_relative_path": "area/root", "inode": 7,
                "record_path": "/state/id"}
    good = rebound_path("/Volumes/a", "/Volumes/b", "/Volumes/a/area/root", identity, "uuid")
    if good != "/Volumes/b/area/root":
        problems.append("rebind did not preserve the relative path")
    for old, record, uuid in [
        ("/Volumes/b/area/root", identity, "uuid"),
        ("/Volumes/a/area/other", identity, "uuid"),
        ("/Volumes/a/area/root", dict(identity, volume_uuid="other"), "uuid"),
    ]:
        try:
            rebound_path("/Volumes/a", "/Volumes/b", old, record, uuid)
            problems.append("rebind accepted a substituted path or identity")
        except Fault:
            pass
    with mock.patch("builtins.open", side_effect=[io.BytesIO(b"id"), io.BytesIO(b"marker"), io.BytesIO(b"content")]), \
            mock.patch("os.stat", return_value=mock.Mock(st_ino=7)):
        verify_rollback(good, identity, b"id", b"marker", b"content", [])
    try:
        verify_rollback(good, identity, b"id", b"marker", b"content", [{"phase": "pending"}])
        problems.append("rollback accepted an uncleared intent")
    except Fault:
        pass
    # Drive the real dispatcher: an R11 fault cannot enter R12's ordinary commands.
    fake = Proof("unused", "unused", True, {})
    for case in CASES:
        setattr(fake, case["method"], lambda: {})
    def fail_remount():
        raise Fault("R11", "synthetic failed oracle")
    setattr(fake, CASES[-2]["method"], fail_remount)
    setattr(fake, CASES[-1]["method"], lambda: problems.append("R12 ran after failed R11"))
    fake.run_cases()
    if fake.results[-1]["status"] != "skipped":
        problems.append("failed R11 did not skip R12")
    # Drive the real R12 body: its failed registration gate must precede all ordinary work.
    stale = Proof("unused", "unused", True, {})
    stale.fresh_area = lambda *args: problems.append("ordinary area operation used the stale hint")
    try:
        stale.case_named_run_refuses_a_root_it_was_not_given()
        problems.append("R12 accepted an uncompleted R11")
    except Fault:
        pass
    with mock.patch("builtins.open", side_effect=[io.BytesIO(b"different"), io.BytesIO(b"marker"), io.BytesIO(b"content")]), \
            mock.patch("os.stat", return_value=mock.Mock(st_ino=7)):
        try:
            verify_rollback(good, identity, b"id", b"marker", b"content", [])
            problems.append("rollback accepted a substituted identity")
        except Fault:
            pass
    return problems


def self_check():
    """The apparatus only: pins, the argv guard, the case table, the feature and the source text."""
    problems = []
    problems += installed_supervisor_problems()
    problems += argv_guard_problems()
    problems += case_table_problems()
    problems += source_problems()
    problems += feature_problems()
    problems += pure_self_check()
    return problems


def plan_document():
    return {
        "schema": SELF_CHECK_SCHEMA,
        "status": "held: no owner activation; no receipt exists",
        "supervisor": {"name": SUPERVISOR_NAME, "sha256": SUPERVISOR_SHA256},
        "build": {
            "feature": PROOF_FEATURE,
            "command": BUILD_COMMAND,
            "staged_under": STAGE_ROOT,
            "stage_selector": "release:native:stage",
            "production_build": "not run by this proof",
        },
        "isolation": {
            "fixture_bindings": sorted(fixture_environment("<home>", "<state>", "<run>")),
            "oxide_directories": {"state": OXIDE_STATE_RELATIVE, "cache": OXIDE_CACHE_RELATIVE},
            "rule": "every bound path resolves inside the supervisor's owned scratch; the reference control each case chose must too",
        },
        "limitation": LIMITATION,
        "premise_dispositions": {
            "private_home_rustup": "setup uses private HOME/CARGO_HOME; no fixture builds; stage_proof_binary uses the pinned repository toolchain and committed lock with auto-install disabled",
            "account_home": "fixture_environment binds HOME and BARGE_ACCOUNT_HOME; setup sets OXIDE_TEST_HOME_FROM_ENV for children",
            "environment": "setup scrubs inherited OXIDE, XDG and BARGE variables, then restores guard and fixture bindings",
            "adoption": "no checkout build in the fixture; roots are created explicitly by new_root",
            "cargo_argv": "no fake Cargo external subcommand is invoked",
            "owned_image": "attach uses owners on; UUID and ownership checked; ordinary detach only; uncertain custody retained",
            "compiler_flags": "stage_proof_binary and fixture setup remove both inherited flag variables",
            "temporary_storage": "setup creates run root inside the guard scratch, image backing inside run root",
            "mktemp": "setup and reset_registration use tempfile.mkdtemp with an explicit owned parent",
        },
        "bounds": {
            "admission": "heavy",
            "image_mib": IMAGE_MIB,
            "wall_seconds": LIMIT_SECONDS,
            "scratch_budget_mib": SCRATCH_BUDGET_MIB,
            "detach": "ordinary",
            "may_delete": "synthetic fixture roots inside the owned image only",
        },
        "commands": {
            "counted": supervisor_command("<supervisor>", "counted", "<staged proof binary>"),
            "diagnostic": supervisor_command("<supervisor>", "diagnostic", "<staged proof binary>"),
        },
        "cases": [{"id": case["id"], "claim": case["claim"], "carried_by": case["carried_by"]}
                  for case in CASES],
        "activation_order": [
            "0. release:native:stage builds and hashes the proof-feature binary of the exact head",
            "1. the owner activates one uncounted diagnostic at the exact head; it proves nothing alone",
            "2. the owner activates one counted author pass at the same head",
            "3. a distinct reviewer activates its own counted pass at the same head",
        ],
    }


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic", "stage"])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--inner", choices=["counted", "diagnostic"])
    parser.add_argument("--binary")
    parser.add_argument("--receipt")
    return parser


def main(argv):
    parser = argument_parser()
    args = parser.parse_args(argv)
    if args.self_check:
        problems = self_check()
        for problem in problems:
            print(f"release-proof self-check: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"release-proof self-check: {len(CASES)} cases carried by named regressions; "
              "the pinned supervisor and the argv guard hold")
        return 0
    if args.plan:
        json.dump(plan_document(), sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0
    if args.inner:
        if not args.binary:
            parser.error("--inner needs --binary")
        return inner_half(args.inner, args.binary, args.receipt)
    if args.mode is None:
        parser.error("name a mode: stage, counted or diagnostic (or --self-check / --plan)")
    if args.mode == "stage":
        try:
            manifest = stage_proof_binary()
        except Infrastructure as error:
            print(json.dumps({"schema": SCHEMA, "ok": False, "refused": f"staging failed: {error.detail}"}))
            return 125
        print(json.dumps({"schema": STAGE_SCHEMA, "ok": True, **manifest}, indent=1))
        return 0
    return outer_half(args.mode)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
