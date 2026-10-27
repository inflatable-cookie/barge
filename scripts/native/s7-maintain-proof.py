#!/usr/bin/env python3
"""Barge S7's native proof: `barge maintain`, preview and real (fixed script).

This is the whole command the elevated native run supervises, in both modes:

  python3 scripts/native/s7-maintain-proof.py counted
  python3 scripts/native/s7-maintain-proof.py diagnostic

It is owner-activated (docs/knowledge/contracts/s7-maintain-proof.md). It is not
a test: no ordinary Effigy selector runs it, and a run below the elevated tier
must not invoke it.

What it does, in one sentence: under Oxide's qualified shared supervisor
(`scripts/bounded-test.py`, bytes pinned below, executed with `/usr/bin/python3`),
it creates one private run root inside the guard's owned scratch, attaches one
owned APFS sparse image of at most 64 MiB with ownership enabled, registers that
image as a fixture volume beside two fixture records whose volumes are *not*
attached, declares one area with a prune policy, and runs the S7 maintain cases
through the real `barge` CLI against a private home and state directory, then
writes its receipt into the supervisor's evidence directory.

What it never does:

  - it never touches the real account home, the real Barge state directory, or a
    registered real drive (Builds included);
  - it never runs Oxide's outputs prune, never reads an Oxide receipt and never
    deletes an Oxide root; case 5 proves the planted fixtures are untouched;
  - it never force-detaches, never changes a file mode on a real path, and never
    writes outside the guard's owned scratch and its evidence directory;
  - it never runs without the supervisor's own contract environment.

Modes:

  counted     Every case must pass; the first fault stops the remaining cases, a
              case whose dependency did not pass is skipped, and the receipt is
              `qualified: true` only on a clean pass.
  diagnostic  The uncounted twin: keep going after a fault, latch the first
              fault, skip a case whose dependency did not pass, and always write
              a `qualified: false` receipt. It proves nothing on its own.
  --self-check  The apparatus only: the case map and its dispatch, the pinned
              supervisor digest, the premise record, the environment inventory
              and the image invocations. No image, no scratch, no child process.

Receipt: $OXIDE_PROOF_EVIDENCE/s7-maintain-receipt.json. A missing receipt is a
missing proof.
"""

import argparse
import ast
import hashlib
import json
import os
import plistlib
import pwd
import shutil
import subprocess
import sys
import tempfile
import time
import traceback

SCHEMA = "barge.maintain.native/1"
SELF_CHECK_SCHEMA = "barge.maintain.native.self-check/1"
SUPERVISOR_NAME = "scripts/bounded-test.py"
# The qualified supervisor's bytes at this proof's authoring head (S4's and S6's
# pin, unchanged: Barge writes no second supervisor).
SUPERVISOR_SHA256 = "bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924"
SCRIPT_PATH = os.path.abspath(__file__)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT_PATH)))
DESIGNATED_SCRATCH = "/Volumes/Builds/scratch/barge"
LIMIT_SECONDS = 300
SCRATCH_BUDGET_MIB = 512
# The brief allows up to two owned images of at most 64 MiB each. This proof
# needs one: the two volumes whose absence the fourth case proves are registered
# records whose mount points are never created, which is the stronger
# counterfactual (a second image would prove only a second present volume).
IMAGE_MIB = 64
# `hdiutil create` does not accept `-owners`; ownership is enabled at attach,
# which is the only place the option exists.
CREATE_IMAGE_OPTIONS = [
    "-size", f"{IMAGE_MIB}m", "-fs", "APFS", "-type", "SPARSE",
    "-volname", "barge-proof-maintain",
]
ATTACH_OPTIONS = ["-nobrowse", "-owners", "on"]
# The product's own relative state root: what premise 2 must see unchanged.
REAL_STATE_RELATIVE = "Library/Application Support/barge/state"
REAL_STATE_ENTRIES = 4096
REAL_STATE_BYTES = 8 * 1024 * 1024
REAL_STATE_SECONDS = 20
# The area's declared prune age, and how far the proof backdates a fixture root.
AREA_AGE_SECONDS = 60
AGED_DAYS = 30
# The native scratch run's age past S4's seven-day native policy.
NATIVE_AGE_DAYS = 8
EXIT_INFRASTRUCTURE = 4

ENVIRONMENT = {
    "TMPDIR": "the guard's owned scratch; every fixture directory is created here",
    "TMP": "the guard's owned scratch, kept equal to TMPDIR",
    "TEMP": "the guard's owned scratch, kept equal to TMPDIR",
    "BARGE_STATE_DIR": "the private fixture state directory this proof creates",
    "BARGE_PROOF_BIN": "the barge binary this proof exercises",
    "BARGE_PROOF_SUPERVISOR": "the optional path to the hash-pinned Oxide supervisor",
    "OXIDE_PROOF_EVIDENCE": "the supervisor's evidence directory for the receipt",
    "OXIDE_TASK_SCRATCH_ROOT": "the supervisor's owned scratch root that TMPDIR must be inside",
    "DYLD_INSERT_LIBRARIES": "discarded from both halves: this proof injects no interposer",
    "HOME": "a private fixture home, so no real account state is read or written",
    "CARGO_HOME": "a private fixture Cargo home; this proof builds nothing",
}

# The six cases, in the brief's order. `needs` names the cases that must have
# passed before this one is meaningful, so a diagnostic run skips dependents
# instead of cascading.
CASES = [
    ("preview_then_real_deletes_exactly_the_planned_units", []),
    ("a_tampered_plan_refuses", ["preview_then_real_deletes_exactly_the_planned_units"]),
    ("a_hold_between_preview_and_real_stops_that_unit",
     ["a_tampered_plan_refuses"]),
    # The absent-volume case rewrites this proof's registry, so it runs only after
    # the planned-deletion cases: a record for a volume that is not attached makes
    # every plan loud (exit 3), which would confuse a case that expects a quiet
    # exit 0. It restores the one-volume registry in a finally block, and the
    # pending-intent case stays last because its intent is replayed by any later
    # real run.
    ("an_oxide_output_root_and_receipt_fixture_are_untouched",
     ["a_hold_between_preview_and_real_stops_that_unit"]),
    ("an_absent_ephemeral_volume_is_skipped_and_a_permanent_one_is_reported",
     ["an_oxide_output_root_and_receipt_fixture_are_untouched"]),
    ("exit_3_with_a_pending_intent",
     ["an_absent_ephemeral_volume_is_skipped_and_a_permanent_one_is_reported"]),
]

PREMISES = [
    "a private HOME, CARGO_HOME and state directory, so no real account state is read or written",
    "the account home used for state resolution is the fixture's, proved from the child environment, and the real state directory is observed completely and asserted unchanged",
    "BARGE_STATE_DIR, TMPDIR, TMP and TEMP all point inside the guard's owned scratch",
    "every fixture volume, area and root is created by this proof's own explicit command, never by a default",
    "one owned sparse image at most 64 MiB attached with -owners on, detached ordinarily, never forced",
    "every environment hook is listed in ENVIRONMENT with its purpose; no debug-only bypass exists",
    "temporary storage is inside the guard's one owned root: no home folder, no drive root, no fixed /tmp name",
    "mktemp -d under that root for every fixture directory, never a shared name",
    "the supervisor's bytes are hash-pinned above and Barge adds no second supervisor, session or accounting harness",
]


def now_ms():
    return int(time.time() * 1000)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def run(command, env, cwd=None, timeout=120, check=True):
    """Run one command, returning its completed process; never a shell."""
    result = subprocess.run(
        command, env=env, cwd=cwd, timeout=timeout, capture_output=True, text=True
    )
    if check and result.returncode != 0:
        raise Fault(
            "command",
            f"{' '.join(command)} exited {result.returncode}: "
            f"{(result.stderr or result.stdout).strip()[-400:]}",
        )
    return result


def account_home():
    """The password-database home for the effective uid, never `$HOME`."""
    return pwd.getpwuid(os.geteuid()).pw_dir


def child_environment_problems(env, state_dir, fixture_home, home):
    """Why a child command's environment cannot prove fixture state resolution."""
    problems = []
    override = env.get("BARGE_STATE_DIR")
    if override != state_dir:
        problems.append(
            f"the child's BARGE_STATE_DIR is {override!r}, not the fixture {state_dir!r}"
        )
    elif not os.path.isabs(override):
        problems.append(f"the child's BARGE_STATE_DIR is not an absolute path: {override!r}")
    if env.get("HOME") != fixture_home:
        problems.append(
            f"the child's HOME is {env.get('HOME')!r}, not the fixture {fixture_home!r}"
        )
    if home == fixture_home:
        problems.append(
            "the fixture HOME is also the password-database account home, so the proof "
            "cannot separate the two state sources"
        )
    return problems


def _incomplete(reason, entries=0, total=0):
    return {
        "status": "incomplete",
        "digest": None,
        "entries": entries,
        "bytes": total,
        "reason": reason,
    }


def real_state_observation(deadline_seconds=REAL_STATE_SECONDS, clock=time.monotonic):
    """Observe the real Barge state directory completely, or say it is incomplete.

    Every name is read through a descriptor with `O_NOFOLLOW`, so a symlinked or
    swapped path is an incomplete observation and never a followed one, and each
    entry's name, kind, mode, size, mtime and bytes are hashed. A cap, a short
    read, a size or mtime change, or a step that finished past the deadline makes
    the observation incomplete; two incomplete observations never prove equality.
    """
    root = os.path.join(account_home(), REAL_STATE_RELATIVE)
    if not os.path.exists(root):
        return {"status": "absent", "digest": None, "entries": 0, "bytes": 0, "reason": None}
    deadline = clock() + deadline_seconds
    digest = hashlib.sha256()
    entries = 0
    total = 0
    stack = [root]
    try:
        while stack:
            if clock() >= deadline:
                return _incomplete("the observation deadline was reached", entries, total)
            directory = stack.pop()
            with os.scandir(directory) as listing:
                for entry in listing:
                    entries += 1
                    if entries > REAL_STATE_ENTRIES:
                        return _incomplete("the entry cap was reached", entries, total)
                    info = entry.stat(follow_symlinks=False)
                    fields = (
                        os.path.relpath(entry.path, root),
                        entry.is_dir(follow_symlinks=False),
                        info.st_mode,
                        info.st_size,
                        info.st_mtime_ns,
                    )
                    digest.update(repr(fields).encode())
                    if entry.is_dir(follow_symlinks=False):
                        stack.append(entry.path)
                        continue
                    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
                    descriptor = os.open(entry.path, flags)
                    try:
                        opened = os.fstat(descriptor)
                        if (opened.st_size, opened.st_mtime_ns) != (info.st_size, info.st_mtime_ns):
                            return _incomplete(
                                "an entry changed between its facts and its open", entries, total
                            )
                        remaining = info.st_size
                        read = 0
                        while remaining > 0:
                            block = os.read(descriptor, min(remaining, 1 << 16))
                            if not block:
                                break
                            digest.update(block)
                            read += len(block)
                            remaining -= len(block)
                            total += len(block)
                            if total > REAL_STATE_BYTES:
                                return _incomplete("the byte cap was reached", entries, total)
                            if clock() >= deadline:
                                return _incomplete(
                                    "the observation deadline passed during a read", entries, total
                                )
                        after = os.fstat(descriptor)
                        if read != info.st_size or (
                            after.st_size,
                            after.st_mtime_ns,
                        ) != (info.st_size, info.st_mtime_ns):
                            return _incomplete("a short or unstable read", entries, total)
                    finally:
                        os.close(descriptor)
    except OSError as error:
        return _incomplete(f"the observation failed: {error}", entries, total)
    if clock() >= deadline:
        return _incomplete("the observation deadline passed before it completed", entries, total)
    return {
        "status": "complete",
        "digest": digest.hexdigest(),
        "entries": entries,
        "bytes": total,
        "reason": None,
    }


def tree_observation(path):
    """One fixture tree's names, kinds, sizes and bytes, for an untouched check."""
    digest = hashlib.sha256()
    entries = 0
    if not os.path.exists(path):
        return {"path": path, "status": "absent", "digest": None, "entries": 0}
    for base, directories, files in os.walk(path):
        directories.sort()
        for name in sorted(files):
            target = os.path.join(base, name)
            info = os.lstat(target)
            digest.update(os.path.relpath(target, path).encode())
            digest.update(repr((info.st_mode, info.st_size, info.st_mtime_ns)).encode())
            with open(target, "rb") as handle:
                digest.update(handle.read())
            entries += 1
    return {"path": path, "status": "complete", "digest": digest.hexdigest(), "entries": entries}


def environment_hooks(source):
    """Find explicitly accessed or assigned environment keys in one proof source.

    The proof reads `os.environ` and writes the child environment dictionaries
    `env` and `self.env`. Copying the inherited environment itself does not name
    a hook; every individual access and child override does.
    """
    hooks = set()
    problems = []

    def environment(node):
        return (isinstance(node, ast.Name) and node.id == "env") or (
            isinstance(node, ast.Attribute)
            and (
                (isinstance(node.value, ast.Name) and node.value.id == "os"
                 and node.attr == "environ")
                or (isinstance(node.value, ast.Name) and node.value.id == "self"
                    and node.attr == "env")
            )
        )

    def add_key(node):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            hooks.add(node.value)
        else:
            problems.append("an environment key is not a literal")

    for node in ast.walk(ast.parse(source)):
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and isinstance(node.func.value, ast.Name)
            and node.func.value.id == "os"
            and node.func.attr == "getenv"
        ):
            if node.args:
                add_key(node.args[0])
            else:
                problems.append("an environment read has no key")
        elif isinstance(node, ast.Subscript) and environment(node.value):
            add_key(node.slice)
        elif (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and environment(node.func.value)
        ):
            if node.func.attr in ("get", "pop") and node.args:
                add_key(node.args[0])
            elif node.func.attr == "update" and not node.args:
                for keyword in node.keywords:
                    if keyword.arg is None:
                        problems.append("an environment update has dynamic keys")
                    else:
                        hooks.add(keyword.arg)
            else:
                problems.append(f"an environment operation cannot be inventoried: {node.func.attr}")
    return hooks, problems


def environment_inventory_problems(sources, declared):
    """Check every hook read or set is declared and every declaration is used."""
    hooks = set()
    problems = []
    for source in sources:
        found, errors = environment_hooks(source)
        hooks.update(found)
        problems.extend(errors)
    for name in sorted(hooks - set(declared)):
        problems.append(f"the environment record omits {name}")
    for name in sorted(set(declared) - hooks):
        problems.append(f"the environment record names an unused hook {name}")
    for name, purpose in declared.items():
        if not isinstance(purpose, str) or not purpose.strip():
            problems.append(f"the environment hook {name} has no recorded purpose")
    return problems


class Fault(Exception):
    def __init__(self, kind, detail):
        super().__init__(detail)
        self.kind = kind
        self.detail = detail


class Infrastructure(Exception):
    def __init__(self, check, detail, record=None):
        super().__init__(detail)
        self.check = check
        self.detail = detail
        self.record = record


class Proof:
    def __init__(self, binary, receipt_path, diagnostic):
        self.binary = binary
        self.receipt_path = receipt_path
        self.diagnostic = diagnostic
        self.root = None
        self.home = None
        self.state = None
        self.mount = None
        self.image = None
        self.device = None
        self.whole_disk = None
        self.uuid = None
        self.absent_ephemeral = None
        self.absent_permanent = None
        self.env = None
        self.results = []
        self.case_evidence = []
        self.fault = None
        self.preflight = None
        self.infrastructure = None
        self.real_state = None
        self.attached = False
        self.detach_evidence = []

    # -- fixture setup ----------------------------------------------------

    def setup(self):
        scratch = os.environ.get("TMPDIR")
        if not scratch or not os.path.isdir(scratch):
            raise Fault("premise", "TMPDIR is not a directory the guard created")
        self.real_state = {
            "path": os.path.join(account_home(), REAL_STATE_RELATIVE),
            "account_home": account_home(),
            "before": real_state_observation(),
            "after": None,
            "complete": False,
            "unchanged": None,
        }
        self.root = tempfile.mkdtemp(prefix="barge-maintain-proof-", dir=scratch)
        self.home = tempfile.mkdtemp(prefix="home-", dir=self.root)
        self.state = tempfile.mkdtemp(prefix="state-", dir=self.root)
        # The two volumes whose absence case 4 proves: fixture mount paths that
        # are named in the registry and never created, ever.
        self.absent_ephemeral = os.path.join(self.root, "absent-ephemeral")
        self.absent_permanent = os.path.join(self.root, "absent-permanent")
        self.env = dict(os.environ)
        self.env.pop("DYLD_INSERT_LIBRARIES", None)
        self.env.update(
            HOME=self.home,
            CARGO_HOME=os.path.join(self.home, "cargo"),
            BARGE_STATE_DIR=self.state,
            TMPDIR=self.root,
            TMP=self.root,
            TEMP=self.root,
        )
        os.makedirs(self.env["CARGO_HOME"], exist_ok=True)
        self.image = os.path.join(self.root, "maintain-volume.sparseimage")
        for name in ("hdiutil", "diskutil"):
            if shutil.which(name) is None:
                raise Fault("premise", f"{name} is not available")
        self.create_image()

    def create_image(self):
        run(["hdiutil", "create", *CREATE_IMAGE_OPTIONS, self.image], self.env)
        name = f"barge-maintain-proof-{os.getpid()}-{int(time.time()) % 100000}"
        self.mount = f"/Volumes/{name}"
        run(
            ["hdiutil", "attach", self.image, "-mountpoint", self.mount, *ATTACH_OPTIONS],
            self.env,
        )
        self.attached = True
        info = plistlib.loads(
            run(["diskutil", "info", "-plist", self.mount], self.env).stdout.encode()
        )
        self.uuid = info.get("VolumeUUID")
        self.device = info.get("DeviceIdentifier")
        self.whole_disk = info.get("ParentWholeDisk")
        if not self.uuid or not self.device or not self.whole_disk:
            raise Fault("premise", "the owned image did not report its UUID, device and whole disk")
        if not str(info.get("Owners", "")).lower() in ("yes", "true", "1"):
            # Ownership is what the brief requires; `diskutil` reports it as a
            # boolean on some releases and as a string on others.
            owners = info.get("Owners")
            if owners not in (True, "Yes", "yes", "1"):
                raise Fault("premise", f"the owned image did not enable ownership: {owners!r}")
        self.register()
        self.declare_area()

    ABSENT_EPHEMERAL_UUID = "B1B2C3D4-1111-4222-8333-944455556666"
    ABSENT_PERMANENT_UUID = "C1C2C3D4-1111-4222-8333-944455556666"

    def volume_record(self, uuid, mount, availability):
        return {
            "volume_uuid": uuid,
            "mount_hint": mount,
            "availability": availability,
            "durability": "expendable",
            "root_dir": "",
            "folders": {"scratch": None, "caches": None},
            "origin": "declared",
            "scratch_grants": {"delegations": [], "claims": []},
            "generation": 1,
            "registered_ms": now_ms(),
        }

    def write_registry(self, volumes):
        """Publish the fixture registry: this proof's own explicit write."""
        with open(os.path.join(self.state, "registry.json"), "w", encoding="utf-8") as handle:
            json.dump({"schema": "barge.registry/1", "volumes": volumes}, handle, indent=1)
            handle.write("\n")

    def register(self):
        """Register the attached volume only, so quiet runs stay quiet.

        The two absent volumes are records the absent-volume case adds and then
        removes: a record for a volume that is not attached makes every plan loud
        (exit `3`), so registering them here would stop every planned-deletion
        case from observing the quiet exit its behaviour is stated against.
        """
        self.write_registry([self.volume_record(self.uuid, self.mount, "permanent")])

    def add_absent_volumes(self):
        """Register the two volumes whose mount points are never created."""
        self.write_registry([
            self.volume_record(self.uuid, self.mount, "permanent"),
            self.volume_record(
                self.ABSENT_EPHEMERAL_UUID, self.absent_ephemeral, "ephemeral"
            ),
            self.volume_record(
                self.ABSENT_PERMANENT_UUID, self.absent_permanent, "permanent"
            ),
        ])

    def restore_attached_only(self):
        """Drop the absent records again, so a later case sees a quiet plan."""
        self.register()

    def declare_area(self):
        self.barge(
            "area", "declare", "--namespace", "example", "--area", "pool",
            "--folder", "pool", "--absent", "wait",
            "--prune", f"oldest-idle:{AREA_AGE_SECONDS}", "--reserve", "counts",
            expect=0,
        )

    # -- the CLI ----------------------------------------------------------

    def barge(self, *args, expect=None, timeout=120):
        command = [self.binary, *args, "--json"]
        result = run(command, self.env, cwd=self.root, timeout=timeout, check=False)
        try:
            document = json.loads(result.stdout)
        except ValueError:
            raise Fault(
                "barge",
                f"{' '.join(args)} printed no JSON (exit {result.returncode}): "
                f"{result.stdout[-300:]}{result.stderr[-300:]}",
            )
        if expect is not None and result.returncode != expect:
            raise Fault(
                "barge",
                f"{' '.join(args)} exited {result.returncode}, expected {expect}: "
                f"{json.dumps(document)[:400]}",
            )
        return result.returncode, document

    def preview(self, expect=0, plan_out=None):
        args = ["maintain"]
        if plan_out:
            args += ["--plan-out", plan_out]
        code, document = self.barge(*args, expect=expect)
        return code, document

    def real(self, digest, plan_file, expect=0):
        """The real form: the preview's exact bytes and their digest."""
        code, document = self.barge(
            "maintain", "--plan", plan_file, "--plan-sha256", digest, expect=expect
        )
        return code, document

    def plan_path(self, case):
        """One preview's plan file, inside this run root."""
        return os.path.join(self.root, f"{case}-plan.json")

    def create_area_root(self, kind="area_root"):
        _code, document = self.barge(
            "area", "root", "create", "--namespace", "example", "--area", "pool",
            "--volume", self.uuid, "--kind", kind, expect=0,
        )
        return document["data"]["root"]

    def create_scratch_run(self, project, consumer):
        _code, document = self.barge(
            "scratch", "alloc", "--volume", self.uuid, "--project", project,
            "--consumer", consumer, "--pid", str(os.getpid()), expect=0,
        )
        root = document["data"]["root"]
        self.barge(
            "scratch", "conclude", "--volume", self.uuid, "--root", root["root_id"],
            "--holder", consumer, "--outcome", "closed", "--closure", "positive",
            "--kept-reason", "evidence", "--nested-scope", "exclusive", expect=0,
        )
        self.barge("area", "release", "--root", root["root_id"], "--holder", consumer, expect=0)
        return root

    def hold(self, root_id, holder):
        self.barge(
            "area", "hold", "--root", root_id, "--holder", holder,
            "--pid", str(os.getpid()), "--scope", "read",
            "--purpose", "the S7 native proof's between-preview-and-real hold",
            expect=0,
        )

    # -- helpers ----------------------------------------------------------

    def area_folder(self):
        return os.path.join(self.mount, "pool")

    def tree_age(self, path, days):
        stamp = time.strftime("%Y%m%d%H%M", time.gmtime(time.time() - days * 86400))
        run(["find", path, "-exec", "touch", "-t", f"{stamp}.00", "{}", "+"], self.env)

    def edit_marker(self, root_path, change):
        """Rewrite one fixture root's marker in place, keeping it private.

        The marker's exact bytes are its identity, so a case that needs an older
        root edits the recorded time and lets the next preview compute the digest
        from the bytes it reads.
        """
        marker = os.path.join(root_path, ".barge-root.json")
        info = os.stat(marker)
        with open(marker, encoding="utf-8") as handle:
            document = json.load(handle)
        change(document)
        with open(marker, "w", encoding="utf-8") as handle:
            json.dump(document, handle, indent=2)
            handle.write("\n")
        os.chmod(marker, info.st_mode & 0o777)

    def age_area_root(self, root_path):
        """Backdate an owned root: its directory mtime and its recorded creation."""
        self.tree_age(root_path, AGED_DAYS)
        stamp = now_ms() - AGED_DAYS * 86400 * 1000

        def change(document):
            document["created_ms"] = stamp

        self.edit_marker(root_path, change)

    def age_native_run(self, root_path):
        """Age a concluded scratch run past S4's seven-day native policy."""
        stamp = now_ms() - NATIVE_AGE_DAYS * 86400 * 1000

        def change(document):
            document["conclusion"]["concluded_ms"] = stamp

        self.edit_marker(root_path, change)

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

    def quarantine_leaves(self, folder):
        if not os.path.isdir(folder):
            return []
        return [
            name for name in sorted(os.listdir(folder)) if name.startswith(".barge-prune-")
        ]

    def volume_row(self, document, uuid):
        for row in document["data"]["plan"]["volumes"]["rows"]:
            if row["volume_uuid"] == uuid:
                return row
        raise Fault("case", f"the plan has no row for volume {uuid}")

    def area_row(self, document, volume_uuid="self"):
        volume = self.volume_row(document, self.uuid if volume_uuid == "self" else volume_uuid)
        if not volume.get("areas", {}).get("rows"):
            raise Fault("case", "the plan has no area step")
        return volume["areas"]["rows"][0]

    def candidate_names(self, document):
        names = []
        for volume in document["data"]["plan"]["volumes"]["rows"]:
            scratch = volume.get("scratch") or {}
            for row in (scratch.get("plan") or {}).get("candidates", {}).get("rows", []):
                names.append(row["name"])
            for area in volume.get("areas", {}).get("rows", []):
                for row in area.get("candidates", {}).get("rows", []):
                    names.append(row["name"])
        return names

    # -- preflight --------------------------------------------------------

    def preflight_census(self):
        """The product's own same-uid observation, through the binary under test.

        The S4 proof owns the census case; here the read-only diagnostic is
        enough to tell a host capability limitation from a product failure. A
        scan that cannot complete is `infrastructure` and no case runs.
        """
        code, document = self.barge(
            "diagnostic", "descriptor-census", "--root", self.mount
        )
        record = {"exit": code, "document": document}
        status = document.get("data", {}).get("status")
        if code == 0 and status == "passed":
            return record
        raise Infrastructure(
            "product_custody_census",
            f"the product's descriptor census did not pass (exit {code}, status {status!r})",
            record,
        )

    # -- cases ------------------------------------------------------------

    def case_preview_then_real_deletes_exactly_the_planned_units(self):
        planned = self.create_area_root()
        self.age_area_root(planned["path"])
        kept = self.create_area_root()
        scratch_run = self.create_scratch_run("proof-alpha", "proof")
        self.age_native_run(scratch_run["path"])

        plan_file = self.plan_path("case1")
        _code, document = self.preview(expect=0, plan_out=plan_file)
        plan = document["data"]["plan"]
        assert plan["schema"] == "barge.maintain.plan/1"
        candidates = self.candidate_names(document)
        assert planned["root_id"] in candidates, candidates
        assert scratch_run["root_id"] in candidates, candidates
        assert kept["root_id"] not in candidates, candidates
        area = self.area_row(document)
        kept_rows = {row["name"]: row["reason"] for row in area["kept"]["rows"]}
        assert kept["root_id"] in kept_rows, kept_rows
        assert "declared" in kept_rows[kept["root_id"]], kept_rows[kept["root_id"]]
        assert document["data"]["candidates"] == len(candidates), document["data"]["candidates"]
        digest = document["data"]["plan_digest"]

        code, real = self.real(digest, plan_file, expect=0)
        assert real["data"]["preview"] is False
        assert real["data"]["plan_digest"] == digest
        assert real["data"]["pending"]["count"] == 0, real["data"]["pending"]
        volume = real["data"]["volumes"]["rows"][0]
        assert volume["scratch"]["deleted"]["count"] == 1, volume["scratch"]
        assert volume["areas"][0]["deleted"]["count"] == 1, volume["areas"][0]
        assert not os.path.exists(planned["path"]), "the planned root is gone"
        assert not os.path.exists(scratch_run["path"]), "the planned scratch run is gone"
        assert os.path.isdir(kept["path"]), "the kept root stays"
        assert os.path.isfile(os.path.join(kept["path"], ".barge-root.json")), kept
        return {
            "exit": code,
            "planned": planned["root_id"],
            "scratch": scratch_run["root_id"],
            "kept": kept["root_id"],
            "candidates": len(candidates),
            "digest": digest,
        }

    def case_a_tampered_plan_refuses(self):
        root = self.create_area_root()
        self.age_area_root(root["path"])
        plan_file = os.path.join(self.root, "case2-plan.json")
        _code, preview = self.preview(expect=0, plan_out=plan_file)
        digest = preview["data"]["plan_digest"]
        with open(plan_file, "rb") as handle:
            saved = handle.read()
        assert hashlib.sha256(saved).hexdigest() == digest, "the preview wrote the exact bytes"
        with open(plan_file, "wb") as handle:
            handle.write(saved + b" ")
        code, document = self.real(digest, plan_file=plan_file, expect=2)
        assert document["error"]["kind"] == "usage", document["error"]
        assert "do not match" in document["error"]["message"], document["error"]["message"]
        assert os.path.isdir(root["path"]), "a tampered plan changed nothing"
        # The preview's own bytes and digest then authorise the run, and it
        # deletes the unit that plan named.
        code_ok, real = self.real(digest, plan_file, expect=0)
        assert real["data"]["preview"] is False
        assert not os.path.exists(root["path"]), "the authorised run deleted the planned root"
        return {"tampered_exit": code, "authorised_exit": code_ok, "digest": digest}

    def case_a_hold_between_preview_and_real_stops_that_unit(self):
        held = self.create_area_root()
        free = self.create_area_root()
        self.age_area_root(held["path"])
        self.age_area_root(free["path"])
        plan_file = self.plan_path("case3")
        _code, preview = self.preview(expect=0, plan_out=plan_file)
        digest = preview["data"]["plan_digest"]
        candidates = self.candidate_names(preview)
        assert held["root_id"] in candidates, candidates
        assert free["root_id"] in candidates, candidates

        self.hold(held["root_id"], "s7-proof-holder")
        # The plan still names both roots. The real run re-verifies each one, so
        # the held root is kept and the other planned root is still deleted: the
        # hold stops that unit only, and the run completes with exit 0.
        code, real = self.real(digest, plan_file, expect=0)
        stage = real["data"]["volumes"]["rows"][0]["areas"][0]
        assert stage["deleted"]["count"] == 1, stage
        assert stage["skipped"]["count"] == 1, stage
        assert stage["skipped"]["rows"][0]["name"] == held["root_id"], stage
        assert "hold" in stage["skipped"]["rows"][0]["reason"], stage["skipped"]["rows"][0]
        assert os.path.isdir(held["path"]), "the held root is kept"
        assert not os.path.exists(free["path"]), "the unheld planned root went"
        assert real["data"]["pending"]["count"] == 0, real["data"]["pending"]

        # A fresh preview reports the held root as kept, so the next run does not
        # plan it either.
        _code, second = self.preview(expect=0)
        assert held["root_id"] not in self.candidate_names(second)
        held_rows = {row["name"]: row["reason"] for row in self.area_row(second)["kept"]["rows"]}
        assert held["root_id"] in held_rows, held_rows
        assert "hold" in held_rows[held["root_id"]], held_rows[held["root_id"]]
        return {
            "exit": code,
            "held": held["root_id"],
            "kept_reason": held_rows[held["root_id"]],
            "skipped": stage["skipped"]["rows"][0]["reason"],
        }

    def case_an_absent_ephemeral_volume_is_skipped_and_a_permanent_one_is_reported(self):
        root = self.create_area_root()
        self.age_area_root(root["path"])
        # The two absent records are this case's own explicit write, and the
        # one-volume registry is restored afterwards so the next case's plan is
        # quiet again.
        self.add_absent_volumes()
        try:
            return self.absent_volume_evidence(root)
        finally:
            self.restore_attached_only()

    def absent_volume_evidence(self, root):
        # The plan is loud because a permanent volume is absent, so the preview
        # and the real run both exit 3 while the attached volume is maintained.
        plan_file = self.plan_path("case5")
        code, preview = self.preview(expect=3, plan_out=plan_file)
        assert code == 3
        ephemeral = self.volume_row(preview, self.ABSENT_EPHEMERAL_UUID)
        permanent = self.volume_row(preview, self.ABSENT_PERMANENT_UUID)
        assert ephemeral["state"] == "absent", ephemeral
        assert ephemeral["loud"] is False, ephemeral
        assert "skipped" in ephemeral["detail"], ephemeral["detail"]
        assert permanent["state"] == "absent", permanent
        assert permanent["loud"] is True, permanent
        assert "permanent volume" in permanent["detail"], permanent["detail"]
        assert not os.path.exists(self.absent_ephemeral), "an absent mount point is never created"
        assert not os.path.exists(self.absent_permanent), "an absent mount point is never created"
        assert root["root_id"] in self.candidate_names(preview), self.candidate_names(preview)

        code_real, real = self.real(preview["data"]["plan_digest"], plan_file, expect=3)
        assert real["data"]["volumes"]["rows"][0]["areas"][0]["deleted"]["count"] == 1, real["data"]
        assert not os.path.exists(root["path"]), "the other volume was still maintained"
        assert not os.path.exists(self.absent_ephemeral), "an absent mount point stays absent"
        assert not os.path.exists(self.absent_permanent), "an absent mount point stays absent"
        return {
            "preview_exit": code,
            "real_exit": code_real,
            "ephemeral": ephemeral["detail"],
            "permanent": permanent["detail"],
        }

    def case_an_oxide_output_root_and_receipt_fixture_are_untouched(self):
        output_root = os.path.join(self.mount, "oxide-outputs", "proof-root")
        os.makedirs(output_root, exist_ok=True)
        with open(os.path.join(output_root, "root.json"), "w", encoding="utf-8") as handle:
            json.dump({"schema": "oxide.outputs.root/1", "root": "proof-root"}, handle)
            handle.write("\n")
        with open(os.path.join(output_root, "artifact.bin"), "wb") as handle:
            handle.write(b"oxide output payload\n")
        receipts = os.path.join(self.state, "oxide", "receipts")
        os.makedirs(receipts, exist_ok=True)
        receipt = os.path.join(receipts, "prune.json")
        with open(receipt, "w", encoding="utf-8") as handle:
            json.dump({"version": 1, "pruned": 0}, handle)
            handle.write("\n")
        before_root = tree_observation(output_root)
        before_receipt = tree_observation(receipts)

        root = self.create_area_root()
        self.age_area_root(root["path"])
        plan_file = self.plan_path("case4")
        _code, preview = self.preview(expect=0, plan_out=plan_file)
        _code, real = self.real(preview["data"]["plan_digest"], plan_file, expect=0)
        assert not os.path.exists(root["path"]), "the run did delete its planned unit"
        after_root = tree_observation(output_root)
        after_receipt = tree_observation(receipts)
        assert before_root["digest"] == after_root["digest"], (before_root, after_root)
        assert before_receipt["digest"] == after_receipt["digest"], (before_receipt, after_receipt)
        assert before_root["entries"] == 2, before_root
        assert real["data"]["pending"]["count"] == 0, real["data"]["pending"]
        return {
            "oxide_root": after_root,
            "oxide_receipts": after_receipt,
            "deleted": root["root_id"],
        }

    def case_exit_3_with_a_pending_intent(self):
        root = self.create_area_root()
        self.age_area_root(root["path"])
        # An entry the deletion must never follow: a symlink inside the unit
        # stops it after its quarantine rename and leaves the intent pending.
        os.symlink("/elsewhere", os.path.join(root["path"], "link"))
        plan_file = self.plan_path("case6")
        _code, preview = self.preview(expect=0, plan_out=plan_file)
        code, real = self.real(preview["data"]["plan_digest"], plan_file, expect=3)
        assert real["data"]["pending"]["count"] == 1, real["data"]["pending"]
        assert real["data"]["volumes"]["rows"][0]["areas"][0]["pending"]["count"] == 1, real["data"]
        pending = self.pending_intents()
        assert len(pending) == 1, pending
        phase = pending[0]["phase"]
        assert phase != "deleted", pending[0]
        leaves = self.quarantine_leaves(self.area_folder())
        assert len(leaves) == 1, leaves
        assert not os.path.exists(root["path"]), "the unit was renamed into its quarantine leaf"
        return {
            "exit": code,
            "pending": real["data"]["pending"],
            "phase": phase,
            "leaf": leaves[0],
            "reason": real["data"]["pending"]["rows"][0]["reason"],
        }

    # -- running ----------------------------------------------------------

    def run_cases(self):
        failed = set()
        for name, needs in CASES:
            if self.diagnostic and any(need in failed for need in needs):
                self.results.append({
                    "case": name,
                    "status": "skipped",
                    "kind": "dependency",
                    "detail": "a case it depends on did not pass",
                })
                failed.add(name)
                continue
            if self.fault and not self.diagnostic:
                self.results.append({
                    "case": name,
                    "status": "skipped",
                    "kind": "latched",
                    "detail": "a previous case failed and the counted mode stops there",
                })
                continue
            started = now_ms()
            try:
                evidence = getattr(self, f"case_{name}")()
                self.results.append({
                    "case": name,
                    "status": "passed",
                    "ms": now_ms() - started,
                    "evidence": evidence,
                })
            except Fault as error:
                self.fault = self.fault or name
                failed.add(name)
                self.results.append({
                    "case": name,
                    "status": "failed",
                    "kind": error.kind,
                    "detail": error.detail,
                    "ms": now_ms() - started,
                })
            except Exception as error:
                self.fault = self.fault or name
                failed.add(name)
                self.results.append({
                    "case": name,
                    "status": "failed",
                    "kind": "exception",
                    "detail": f"{type(error).__name__}: {error}",
                    "trace": traceback.format_exc()[-800:],
                    "ms": now_ms() - started,
                })

    def receipt(self, cancelled):
        passed = [row for row in self.results if row["status"] == "passed"]
        failed = [row for row in self.results if row["status"] == "failed"]
        skipped = [row for row in self.results if row["status"] == "skipped"]
        qualified = (
            not self.diagnostic
            and not failed
            and not skipped
            and len(passed) == len(CASES)
            and self.fault is None
            and cancelled is None
            and self.infrastructure is None
        )
        return {
            "schema": SCHEMA,
            "mode": "diagnostic" if self.diagnostic else "counted",
            "qualified": qualified,
            "classification": (
                "infrastructure"
                if self.infrastructure
                else ("pass" if qualified else "unqualified")
            ),
            "started_ms": now_ms(),
            "activated_by": "the owner, separately for each run",
            "supervisor": {
                "path": os.path.join(
                    os.path.dirname(os.path.dirname(SCRIPT_PATH)), SUPERVISOR_NAME
                ),
                "sha256": SUPERVISOR_SHA256,
                "name": SUPERVISOR_NAME,
                "limit_seconds": LIMIT_SECONDS,
                "scratch_budget_mib": SCRATCH_BUDGET_MIB,
            },
            "binary": {"path": self.binary, "sha256": sha256_file(self.binary)},
            "image": {
                "backing": self.image,
                "mount": self.mount,
                "volume_uuid": self.uuid,
                "device": self.device,
                "whole_disk": self.whole_disk,
                "mib": IMAGE_MIB,
                "attach": ["hdiutil", "attach", "<image>", "-mountpoint", "<mount>", *ATTACH_OPTIONS],
                "detach": self.detach_evidence,
            },
            "environment": {
                "hooks": ENVIRONMENT,
                "child": child_environment_problems(
                    self.env or {}, self.state, self.home, account_home()
                ),
            },
            "premises": PREMISES,
            "premise_trace": premise_trace(),
            "preflight": self.preflight,
            "real_state": self.real_state,
            "cases": self.results,
            "case_evidence": self.case_evidence,
            "infrastructure": self.infrastructure,
            "fault": self.fault,
            "cancelled": cancelled,
            "diagnostic": self.diagnostic,
        }

    def write_receipt(self, receipt):
        with open(self.receipt_path, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=1)
            handle.write("\n")

    def cleanup(self):
        """Ordinary detach, then remove only this run's own root."""
        cancelled = None
        if self.attached and self.mount:
            before = diskutil_info(self.mount, self.env)
            if before is not None and (
                before.get("VolumeUUID") != self.uuid
                or before.get("DeviceIdentifier") != self.device
            ):
                cancelled = "the mounted volume is no longer the owned image; detach refused"
            else:
                result = run(
                    ["hdiutil", "detach", self.mount], self.env, check=False, timeout=120
                )
                self.detach_evidence.append({
                    "mount": self.mount,
                    "exit": result.returncode,
                    "forced": False,
                })
                if result.returncode != 0:
                    cancelled = (
                        "an ordinary detach of the owned image failed; the image and its "
                        f"backing are kept: {result.stderr.strip()[-200:]}"
                    )
                else:
                    self.attached = False
        if self.root and os.path.isdir(self.root):
            if cancelled is None and not self.attached:
                shutil.rmtree(self.root, ignore_errors=True)
        return cancelled


def diskutil_info(mount, env):
    result = run(["diskutil", "info", "-plist", mount], env, check=False, timeout=60)
    if result.returncode != 0:
        return None
    try:
        return plistlib.loads(result.stdout.encode())
    except Exception:
        return None


PREMISE_TRACE = [
    ("a private HOME, CARGO_HOME and state directory", "setup", "self.env sets HOME, CARGO_HOME and BARGE_STATE_DIR under the run root"),
    ("the fixture account home and an unchanged real state", "premise_2", "real_state_observation before and after, and child_environment_problems"),
    ("environment isolation", "setup", "TMPDIR, TMP and TEMP are the run root; BARGE_STATE_DIR is the fixture state"),
    ("explicit adoption only", "setup", "register() writes the three records and declare_area() declares the area by command"),
    ("the owned image", "create_image", "-owners on at attach, one ordinary detach in cleanup, no force path in this script"),
    ("every environment hook is listed", "self_check", "environment_inventory_problems over this script's sources"),
    ("temporary storage inside the owned root", "setup", "tempfile.mkdtemp with dir=the run root, which is inside TMPDIR"),
    ("mktemp -d for every fixture directory", "all cases", "root creation uses tempfile.mkdtemp; no fixed path"),
    ("the pinned supervisor, and no second harness", "main", "the outer half verifies the pinned digest and execs it"),
]


def premise_trace():
    """The nine-premise checklist, traced to the body that carries each one."""
    return [
        {"premise": premise, "body": body, "assertion": assertion}
        for premise, body, assertion in PREMISE_TRACE
    ]


def preflight_problems():
    problems = []
    if len(PREMISE_TRACE) != 9 or len(PREMISES) != 9:
        problems.append("the premise record does not name nine premises")
    if len(SUPERVISOR_SHA256) != 64:
        problems.append("the supervisor digest is not a SHA-256")
    if IMAGE_MIB > 64:
        problems.append("the owned image is larger than the brief's 64 MiB")
    if f"{IMAGE_MIB}m" not in CREATE_IMAGE_OPTIONS:
        problems.append("the create invocation's size is not the bounded image size")
    if CREATE_IMAGE_OPTIONS[-2:] != ["-volname", "barge-proof-maintain"]:
        problems.append("the create invocation's volume name changed")
    if "-owners" in CREATE_IMAGE_OPTIONS:
        problems.append("`hdiutil create` is given `-owners`, which it does not accept")
    if ATTACH_OPTIONS.count("-owners") != 1:
        problems.append("the attach does not enable ownership exactly once")
    else:
        index = ATTACH_OPTIONS.index("-owners")
        if index + 1 >= len(ATTACH_OPTIONS) or ATTACH_OPTIONS[index + 1] != "on":
            problems.append("the attach's `-owners` is not followed by `on`")
    # The inventory is checked over the declaring file itself, exactly as the
    # environment-hook rule counts it.
    sources = []
    for name in ("s7-maintain-proof.py",):
        with open(os.path.join(os.path.dirname(SCRIPT_PATH), name), encoding="utf-8") as handle:
            sources.append(handle.read())
    problems.extend(environment_inventory_problems(sources, ENVIRONMENT))
    if not environment_inventory_problems(['os.environ.get("EXAMPLE")'], {}):
        problems.append("an omitted environment hook is not detected")
    if not environment_inventory_problems(['os.environ.get(variable)'], {}):
        problems.append("a dynamic environment hook is not detected")
    with open(SCRIPT_PATH, encoding="utf-8") as handle:
        source = handle.read()
    tree = ast.parse(source)
    # The absent-volume case must run after the planned-deletion cases, and the
    # pending-intent case last: a record for a volume that is not attached makes
    # every plan loud, and a pending intent is replayed by any later real run.
    # A self-check reports problems and never raises: a renamed or missing case
    # is a problem to state, not an exception to escape from.
    positions = {name: index for index, (name, _) in enumerate(CASES)}
    absent_name = "an_absent_ephemeral_volume_is_skipped_and_a_permanent_one_is_reported"
    oxide_name = "an_oxide_output_root_and_receipt_fixture_are_untouched"
    hold_name = "a_hold_between_preview_and_real_stops_that_unit"
    pending_name = "exit_3_with_a_pending_intent"
    required = (absent_name, oxide_name, hold_name, pending_name)
    missing = [name for name in required if name not in positions]
    if missing:
        problems.append(f"the case map is missing a case the ordering rule names: {missing}")
    else:
        if positions[oxide_name] > positions[absent_name]:
            problems.append("the absent-volume case runs before a planned-deletion case")
        if positions[hold_name] > positions[absent_name]:
            problems.append("the absent-volume case runs before the hold case")
        if positions[pending_name] != max(positions.values()):
            problems.append("the pending-intent case is not last")
    methods = {
        node.name: node for node in ast.walk(tree) if isinstance(node, ast.FunctionDef)
    }
    absent_case = methods.get(f"case_{absent_name}")
    if absent_case is None:
        problems.append(f"`case_{absent_name}` is missing")
    else:
        restores = any(
            isinstance(handler, ast.Try)
            and any(
                isinstance(call, ast.Call)
                and isinstance(call.func, ast.Attribute)
                and call.func.attr == "restore_attached_only"
                for statement in handler.finalbody
                for call in ast.walk(statement)
            )
            for handler in ast.walk(absent_case)
        )
        if not restores:
            problems.append(
                "the absent-volume case does not restore the one-volume registry in a "
                "finally block"
            )
    adders = sorted(
        name
        for name, method in methods.items()
        if any(
            isinstance(call, ast.Call)
            and isinstance(call.func, ast.Attribute)
            and call.func.attr == "add_absent_volumes"
            for call in ast.walk(method)
        )
    )
    if adders != [f"case_{absent_name}"]:
        problems.append(f"the absent volumes are registered outside their case: {adders}")
    for node in ast.walk(tree):
        if isinstance(node, (ast.List, ast.Tuple)):
            values = [
                element.value
                for element in node.elts
                if isinstance(element, ast.Constant)
            ]
            if any(
                left == "detach" and right == "-force"
                for left, right in zip(values, values[1:])
            ):
                problems.append("this proof contains a force detach")
    return problems


def self_check():
    """The proof's own apparatus: the case map and its dispatch, pins, premises."""
    problems = preflight_problems()
    names = [name for name, _ in CASES]
    if len(names) != 6 or len(set(names)) != 6:
        problems.append("the case map does not name six distinct cases")
    known = set(names)
    seen = set()
    for index, (name, needs) in enumerate(CASES):
        expected = [] if index == 0 else [CASES[index - 1][0]]
        if needs != expected:
            problems.append(f"{name} does not follow the linear dependency chain")
        for need in needs:
            if need not in known:
                problems.append(f"{name} depends on unknown case {need}")
            elif need not in seen:
                problems.append(f"{name} depends on {need}, which is not evaluated first")
        method = f"case_{name}"
        if not callable(getattr(Proof, method, None)):
            problems.append(f"{name} has no dispatch method `{method}`")
        seen.add(name)
    for attribute in dir(Proof):
        if attribute.startswith("case_") and attribute[len("case_"):] not in known:
            problems.append(f"`{attribute}` is a case method the map never dispatches")
    fixtures = {
        "TMPDIR": "/guard/scratch",
        "BARGE_STATE_DIR": "/guard/scratch/state",
        "HOME": "/guard/scratch/home",
    }
    if child_environment_problems(fixtures, "/guard/scratch/state", "/guard/scratch/home", "/Users/real"):
        problems.append("a child environment inside the guard did not pass")
    if not child_environment_problems(fixtures, "/elsewhere/state", "/guard/scratch/home", "/Users/real"):
        problems.append("a child state directory outside the fixture did not refuse")
    if not child_environment_problems(fixtures, "/guard/scratch/state", "/guard/scratch/home", "/guard/scratch/home"):
        problems.append("a fixture home equal to the account home did not refuse")
    return problems


def outer_half(mode, binary):
    """Verify the pinned supervisor and hand this script to it as the command."""
    supervisor = os.environ.get(
        "BARGE_PROOF_SUPERVISOR",
        os.path.expanduser("~/Dev/projects/oxide/scripts/bounded-test.py"),
    )
    if not os.path.isfile(supervisor) or not os.access(supervisor, os.R_OK):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": f"the qualified supervisor is not a readable regular file at {supervisor}",
        }))
        return 125
    digest = sha256_file(supervisor)
    if digest != SUPERVISOR_SHA256:
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": f"{supervisor} is sha256 {digest}, not the qualified {SUPERVISOR_SHA256}",
        }))
        return 125
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": f"{binary} is not an executable barge binary; build it with `oxide build -p barge-cli --locked`",
        }))
        return 125
    if not os.path.isdir(DESIGNATED_SCRATCH):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": f"the designated scratch parent is not present: {DESIGNATED_SCRATCH}",
        }))
        return 125
    env = dict(os.environ)
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env["BARGE_PROOF_BIN"] = binary
    command = [
        sys.executable, supervisor,
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", DESIGNATED_SCRATCH,
        "--", sys.executable, SCRIPT_PATH, "--inner", mode, binary,
    ]
    result = subprocess.run(command, env=env)
    return result.returncode


def inner_half(mode, binary, receipt_path):
    """The guarded run: the cases, then the receipt."""
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE")
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "")
    if not evidence or not os.path.isdir(evidence):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": "OXIDE_PROOF_EVIDENCE is not a directory; this is not the guard's run",
        }))
        return 125
    if not scratch or not os.path.isdir(scratch):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": "OXIDE_TASK_SCRATCH_ROOT is unset or absent; this is not the guard's run",
        }))
        return 125
    if not os.path.realpath(os.environ.get("TMPDIR", "")).startswith(os.path.realpath(scratch)):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": "TMPDIR is not inside the guard's owned scratch",
        }))
        return 125
    if receipt_path is None:
        receipt_path = os.path.join(evidence, "s7-maintain-receipt.json")
    proof = Proof(binary, receipt_path, mode == "diagnostic")
    cancelled = None
    try:
        proof.setup()
        proof.preflight = proof.preflight_census()
        proof.run_cases()
    except Infrastructure as error:
        proof.preflight = error.record
        proof.infrastructure = {"check": error.check, "detail": error.detail}
        proof.fault = error.check
    except Fault as error:
        proof.results.append({
            "case": "setup",
            "status": "failed",
            "kind": error.kind,
            "detail": error.detail,
        })
        proof.fault = "setup"
    except Exception as error:
        proof.results.append({
            "case": "setup",
            "status": "failed",
            "kind": "exception",
            "detail": f"{type(error).__name__}: {error}",
            "trace": traceback.format_exc()[-800:],
        })
        proof.fault = "setup"
    finally:
        try:
            cancelled = proof.cleanup()
        except Exception as error:
            cancelled = f"the proof cleanup raised: {type(error).__name__}: {error}"

    premise_failure = None
    if proof.real_state is not None:
        before = proof.real_state["before"]
        after = real_state_observation()
        proof.real_state["after"] = after
        complete = before["status"] in ("absent", "complete") and after["status"] in (
            "absent",
            "complete",
        )
        proof.real_state["complete"] = complete
        if not complete:
            premise_failure = (
                "the real account state observation is incomplete: "
                f"before {before['status']} ({before['reason']}), after "
                f"{after['status']} ({after['reason']})"
            )
            proof.real_state["unchanged"] = False
        else:
            proof.real_state["unchanged"] = (
                before["status"] == after["status"] and before["digest"] == after["digest"]
            )
            if not proof.real_state["unchanged"]:
                premise_failure = (
                    "the real account state changed during the run: "
                    f"{before['status']}:{before['digest']} -> "
                    f"{after['status']}:{after['digest']}"
                )
    else:
        premise_failure = "the real account state was never observed before the run"

    receipt = proof.receipt(cancelled)
    receipt["premise_failure"] = premise_failure
    receipt["receipt_path"] = proof.receipt_path
    if premise_failure:
        receipt["qualified"] = False
        if receipt["classification"] != "infrastructure":
            receipt["classification"] = "unqualified"
    proof.write_receipt(receipt)
    print(json.dumps({
        "schema": receipt["schema"],
        "qualified": receipt["qualified"],
        "classification": receipt["classification"],
        "infrastructure": receipt["infrastructure"],
        "preflight": receipt["preflight"],
        "premise_failure": receipt["premise_failure"],
        "cancelled": receipt["cancelled"],
        "diagnostic": receipt["diagnostic"],
        "receipt_path": receipt["receipt_path"],
        "cases": receipt["cases"],
    }, indent=1))
    if receipt["qualified"]:
        return 0
    if receipt["classification"] == "infrastructure":
        return EXIT_INFRASTRUCTURE
    return 3 if mode == "diagnostic" else 1


def main(argv):
    parser = argparse.ArgumentParser(description="Barge's maintain native proof")
    parser.add_argument("mode", nargs="?", default=None, choices=[None, "counted", "diagnostic"])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--inner", default=None, choices=[None, "counted", "diagnostic"])
    parser.add_argument("--binary", default=None)
    parser.add_argument("--receipt", default=None)
    options = parser.parse_args(argv)

    if options.self_check:
        problems = self_check()
        print(json.dumps({
            "schema": SELF_CHECK_SCHEMA,
            "cases": [name for name, _ in CASES],
            "dispatch": {name: f"case_{name}" for name, _ in CASES},
            "premises": len(PREMISES),
            "premise_trace": premise_trace(),
            "image": {
                "create": ["hdiutil", "create", *CREATE_IMAGE_OPTIONS, "<image>"],
                "attach": ["hdiutil", "attach", "<image>", "-mountpoint", "<mount>", *ATTACH_OPTIONS],
                "mib": IMAGE_MIB,
            },
            "supervisor": {"name": SUPERVISOR_NAME, "sha256": SUPERVISOR_SHA256},
            "environment": ENVIRONMENT,
            "problems": problems,
            "ok": not problems,
        }, indent=1))
        return 0 if not problems else 1

    if options.inner:
        binary = options.binary or os.environ.get("BARGE_PROOF_BIN", "")
        if not binary or not os.access(binary, os.X_OK):
            print(json.dumps({
                "schema": SCHEMA,
                "ok": False,
                "refused": "BARGE_PROOF_BIN must name an executable barge binary",
            }))
            return 125
        return inner_half(options.inner, binary, options.receipt)

    mode = options.mode or "counted"
    binary = options.binary or os.environ.get(
        "BARGE_PROOF_BIN",
        os.path.join(REPO_ROOT, "target", "debug", "barge"),
    )
    problems = self_check()
    if problems:
        print(json.dumps({"schema": SCHEMA, "ok": False, "problems": problems}))
        return 2
    return outer_half(mode, binary)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
