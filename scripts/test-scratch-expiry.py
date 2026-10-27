#!/usr/bin/env python3
"""The one bounded native proof for Barge's scratch expiry and deletion (S4).

This is a fixed case script, and the only thing the qualified shared supervisor
runs: it creates one owned APFS sparse image inside the guard's scratch, attaches
it at a unique `/Volumes/<name>`, registers that image's volume in a private
fixture state directory, and exercises the nine cases the frozen stage 1 design
names. It never touches a real drive, the real account state, Oxide's or Effigy's
real roots, the host reaper or any LaunchAgent.

Modes:
  (default)      fail-fast; a full pass writes a `qualified: true` receipt.
  --diagnostic   record a recoverable fault, latch the first one, skip only its
                 dependents, and write a `qualified: false` receipt.
  --self-check   check this script's own case map, premise record, preflight and
                 pinned supervisor digest; no image, no scratch, no child.

Before any case, the product's own same-effective-uid descriptor census runs:
the proof never observes the host itself, so the Barge binary does. One probe
root is allocated, positively concluded, released and aged in the guard's owned
image, then really deleted by `barge scratch expire`; that deletion's custody
runs the same scan — same observer, bounds, foreign-uid exclusion,
exited-process handling and bounded EPERM re-observation — every case's deletion
uses. A product custody refusal (an unreadable same-uid process or a scan bound)
is classified `infrastructure`, unqualified, and aborted before any case (exit
4) with the product's own reason in the receipt. Premise 2 is asserted
separately: the real account state is observed before and after, and the child
environment's fixture state resolution is proved from a fixture-only
delegate/revoke pair.

Every environment hook it reads is listed in `ENVIRONMENT` and recorded in the
receipt, because premise 6 requires naming each one.
"""

import argparse
import ast
import hashlib
import inspect
import json
import os
import plistlib
import pwd
import shutil
import stat as stat_module
import subprocess
import sys
import tempfile
import time
import traceback

SCHEMA = "barge.scratch.expiry.native/1"
SUPERVISOR = "scripts/bounded-test.py"
# The qualified supervisor's bytes at this proof's authoring head.
SUPERVISOR_SHA256 = "bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924"
IMAGE_MIB = 128
# The owned image's two invocations, so the code and the self-check cannot drift
# apart. `hdiutil create` does not accept `-owners`; ownership is enabled when
# the image is attached, which is the only place the option exists.
CREATE_IMAGE_OPTIONS = [
    "-size", f"{IMAGE_MIB}m", "-fs", "APFS", "-type", "SPARSE",
    "-volname", "barge-proof-scratch",
]
ATTACH_OPTIONS = ["-nobrowse", "-owners", "on"]
WALL_MS = 300_000
SCRATCH_MIB = 512
NATIVE_AGE_DAYS = 7
# The one preflight check: the product's own same-effective-uid descriptor
# census, which the Barge binary performs. The proof never observes the host
# itself, so the scan's execution identity is the binary the cases use.
PREFLIGHT_CHECKS = ["product_custody_census"]
# The probe unit the census runs through. It is an ordinary native root in the
# guard's owned image: one allocation, one positive conclusion and one real
# deletion, so the scan is the product's own custody decision and nothing else.
PREFLIGHT_PROJECT = "barge-proof-preflight"
PREFLIGHT_CONSUMER = "barge-proof-preflight"
# The product's own scan bounds, as the S5 design declares them: the proof sets
# none of its own, and the receipt names this as the bound the binary applies.
PRODUCT_CUSTODY_BOUNDS = (
    "the product's own: 4096 processes, 65536 descriptors, 10 s per scan, and "
    "the bounded same-uid EPERM re-observation (three retries inside a 250 ms "
    "settle window)"
)
# Exit status for an infrastructure classification: the product could not complete
# its same-uid census, so no case ran and nothing here is evidence.
EXIT_INFRASTRUCTURE = 4
CLAIM_IDLE_SECONDS = 14 * 24 * 60 * 60
EFFIGY_KEEP_SECONDS = 24 * 60 * 60

ENVIRONMENT = {
    "TMPDIR": "the guard's owned scratch; every fixture directory is created here",
    "TMP": "the guard's owned scratch, kept equal to TMPDIR",
    "TEMP": "the guard's owned scratch, kept equal to TMPDIR",
    "BARGE_STATE_DIR": "the private fixture state directory this proof creates",
    "BARGE_PROOF_BIN": "the barge binary this proof exercises",
    "BARGE_PROOF_SUPERVISOR": "the optional path to the hash-pinned Oxide supervisor",
    "OXIDE_PROOF_EVIDENCE": "the supervisor's evidence directory for the default receipt",
    "HOME": "a private fixture home, so no real account state is read or written",
    "CARGO_HOME": "a private fixture Cargo home; this proof builds nothing",
}

# The nine cases, in the design's order. `needs` names the cases that must have
# passed before this one is meaningful, so a diagnostic run can skip dependents
# instead of cascading.
CASES = [
    ("two_concurrent_allocations_get_distinct_ids", []),
    ("one_root_concluded_one_left_unconcluded", ["two_concurrent_allocations_get_distinct_ids"]),
    ("a_synthetic_old_conclusion_expires", ["one_root_concluded_one_left_unconcluded"]),
    ("an_unknown_entry_stops_the_delete", ["a_synthetic_old_conclusion_expires"]),
    ("a_clean_root_is_deleted_and_the_marker_goes_last", ["an_unknown_entry_stops_the_delete"]),
    ("an_injected_enotempty_leaves_a_pending_intent", ["a_clean_root_is_deleted_and_the_marker_goes_last"]),
    ("a_pull_mid_delete_leaves_a_pending_intent_then_replay", ["an_injected_enotempty_leaves_a_pending_intent"]),
    ("a_stale_mount_point_is_never_written", ["a_pull_mid_delete_leaves_a_pending_intent_then_replay"]),
    ("foreign_and_unmarked_units_go_only_under_grants", ["a_stale_mount_point_is_never_written"]),
]

PREMISES = [
    "a private HOME, CARGO_HOME and state directory, so no real account state is read or written",
    "the account home used for state resolution is the fixture's, proved from the child environment and the fixture registry; the real state directory is observed completely and asserted unchanged",
    "BARGE_STATE_DIR, TMPDIR, TMP and TEMP all point inside the guard's owned scratch",
    "every fixture volume is registered by this proof's own explicit registry write, never by a default",
    "one owned sparse image with -owners on, attached at a unique /Volumes name; ordinary detach with the backing kept on failure, and no forced detach at all",
    "every environment hook is listed in ENVIRONMENT with its purpose; no debug-only bypass exists",
    "temporary storage is inside the guard's one owned root: no home folder and no drive root",
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


# The product's own relative state root (crates/barge/src/os/macos.rs): the real
# account state this proof must leave untouched, and the one premise 2 names.
REAL_STATE_RELATIVE = "Library/Application Support/barge/state"
# The bounds of the before/after real-state observation: names, bytes and time.
REAL_STATE_ENTRIES = 4096
REAL_STATE_BYTES = 8 * 1024 * 1024
REAL_STATE_SECONDS = 20


def account_home():
    """The password-database home for the effective uid, never `$HOME`."""
    return pwd.getpwuid(os.geteuid()).pw_dir


def child_environment_problems(env, state_dir, fixture_home, account_home):
    """Why a child command's environment cannot prove fixture state resolution.

    The proof hands its child barge commands `self.env`; the guard's own
    environment is not what the product sees, so only the child's variables are
    evidence. An empty list means the child can resolve state only through this
    proof's fixture state directory, and the account home cannot be confused with
    the fixture home.
    """
    problems = []
    override = env.get("BARGE_STATE_DIR")
    if override != state_dir:
        problems.append(
            f"the child's BARGE_STATE_DIR is {override!r}, not the fixture {state_dir!r}"
        )
    elif not os.path.isabs(override):
        problems.append(
            f"the child's BARGE_STATE_DIR is not an absolute path: {override!r}"
        )
    if env.get("HOME") != fixture_home:
        problems.append(
            f"the child's HOME is {env.get('HOME')!r}, not the fixture {fixture_home!r}"
        )
    if account_home == fixture_home:
        problems.append(
            "the fixture HOME is also the password-database account home, so the "
            "proof cannot separate the two state sources"
        )
    return problems


def file_read_complete(
    *,
    expected_size,
    expected_mtime_ns,
    opened_size,
    opened_mtime_ns,
    read_total,
    after_size,
    after_mtime_ns,
):
    """Whether one file's bytes are a complete, stable reading.

    A short read (the file truncated between its facts and its bytes), an extra
    byte (a file that grew under the read), or any size or mtime change from the
    entry's own facts through the read to its end leaves the content equality
    premise unproven, however plausible the digest looks.
    """
    return (
        expected_size == opened_size == after_size
        and expected_mtime_ns == opened_mtime_ns == after_mtime_ns
        and read_total == expected_size
    )


def file_read_status(clock, deadline, **facts):
    """`complete`, `incomplete` or `deadline` for one file's read.

    A complete read still has to happen inside the observation's bound: a read
    that finishes after the deadline is past it, whatever its bytes say, so the
    observation is incomplete. `clock` and `deadline` are injected so the
    late-read counterfactual can be simulated exactly.
    """
    if not file_read_complete(**facts):
        return "incomplete"
    if clock() >= deadline:
        return "deadline"
    return "complete"


def _incomplete(reason, entries=0, total=0):
    """An observation that cannot prove the state's content: never a pass."""
    return {
        "status": "incomplete",
        "digest": None,
        "entries": entries,
        "bytes": total,
        "reason": reason,
    }


def real_state_observation(deadline_seconds=REAL_STATE_SECONDS, clock=time.monotonic):
    """A bounded, read-only, descriptor-relative observation of the real state.

    Premise 2 needs *complete content equality*, so this walks the account state
    directory through descriptors: the root is opened with `O_NOFOLLOW` and every
    name is opened relative to its parent's descriptor with `O_NOFOLLOW`, so a
    symlinked or swapped path is an incomplete observation and never a followed
    one. Each entry's name, kind, mode, size, mtime and bytes are hashed. A cap,
    the deadline, a short read or an entry whose identity changed under the walk
    makes the observation `incomplete`; an incomplete observation can never prove
    the state unchanged, whichever two incomplete readings agree.
    """
    root = os.path.join(account_home(), REAL_STATE_RELATIVE)
    try:
        root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return {
            "status": "absent",
            "digest": None,
            "entries": 0,
            "bytes": 0,
            "reason": None,
        }
    except OSError as error:
        return _incomplete(f"the real state directory cannot be opened without following: {error}")
    digest = hashlib.sha256()
    deadline = clock() + deadline_seconds
    entries = 0
    total = 0
    stack = [(root_fd, "")]
    try:
        while stack:
            fd, relative = stack.pop()
            try:
                try:
                    with os.scandir(fd) as listing:
                        children = sorted(listing, key=lambda entry: entry.name)
                except OSError as error:
                    return _incomplete(
                        f"`{relative or '.'}` cannot be listed: {error}", entries, total
                    )
                for entry in children:
                    if entries >= REAL_STATE_ENTRIES:
                        return _incomplete(
                            f"the entry cap {REAL_STATE_ENTRIES} was reached", entries, total
                        )
                    if clock() >= deadline:
                        return _incomplete(
                            f"the deadline {deadline_seconds:g}s was reached", entries, total
                        )
                    entries += 1
                    name = os.path.join(relative, entry.name) if relative else entry.name
                    try:
                        info = entry.stat(follow_symlinks=False)
                    except OSError as error:
                        return _incomplete(f"`{name}` changed while it was read: {error}", entries, total)
                    digest.update(name.encode("utf-8", "surrogateescape"))
                    digest.update(b"\0")
                    digest.update(str(info.st_mode & 0o170000).encode())
                    digest.update(b"\0")
                    digest.update(str(info.st_mode & 0o7777).encode())
                    digest.update(b"\0")
                    digest.update(str(info.st_size).encode())
                    digest.update(b"\0")
                    digest.update(str(info.st_mtime_ns).encode())
                    digest.update(b"\0")
                    try:
                        if stat_module.S_ISDIR(info.st_mode):
                            child = os.open(
                                entry.name,
                                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                dir_fd=fd,
                            )
                            opened = os.fstat(child)
                            if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
                                os.close(child)
                                return _incomplete(
                                    f"`{name}` was replaced between its facts and its open",
                                    entries,
                                    total,
                                )
                            stack.append((child, name))
                        elif stat_module.S_ISREG(info.st_mode):
                            file_fd = os.open(
                                entry.name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd
                            )
                            try:
                                opened = os.fstat(file_fd)
                                if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
                                    return _incomplete(
                                        f"`{name}` was replaced between its facts and its open",
                                        entries,
                                        total,
                                    )
                                read_total = 0
                                while True:
                                    block = os.read(file_fd, 1 << 20)
                                    if not block:
                                        break
                                    read_total += len(block)
                                    total += len(block)
                                    if total > REAL_STATE_BYTES:
                                        return _incomplete(
                                            f"the byte cap {REAL_STATE_BYTES} was exceeded",
                                            entries,
                                            total,
                                        )
                                    digest.update(block)
                                after = os.fstat(file_fd)
                                status = file_read_status(
                                    clock,
                                    deadline,
                                    expected_size=info.st_size,
                                    expected_mtime_ns=info.st_mtime_ns,
                                    opened_size=opened.st_size,
                                    opened_mtime_ns=opened.st_mtime_ns,
                                    read_total=read_total,
                                    after_size=after.st_size,
                                    after_mtime_ns=after.st_mtime_ns,
                                )
                                if status == "incomplete":
                                    return _incomplete(
                                        f"`{name}` was truncated or changed during its read "
                                        f"({read_total} bytes read, {info.st_size} expected)",
                                        entries,
                                        total,
                                    )
                                if status == "deadline":
                                    return _incomplete(
                                        f"the read of `{name}` finished after the deadline "
                                        f"{deadline_seconds:g}s",
                                        entries,
                                        total,
                                    )
                            finally:
                                os.close(file_fd)
                        else:
                            digest.update(b"special")
                    except OSError as error:
                        return _incomplete(f"`{name}` cannot be opened without following: {error}", entries, total)
            finally:
                os.close(fd)
    finally:
        for fd, _relative in stack:
            os.close(fd)
    # A complete walk that finished past its bound is not a complete observation:
    # the last listing or filesystem step may itself have blocked.
    if clock() >= deadline:
        return _incomplete(
            f"the observation finished after the deadline {deadline_seconds:g}s",
            entries,
            total,
        )
    return {
        "status": "complete",
        "digest": digest.hexdigest(),
        "entries": entries,
        "bytes": total,
        "reason": None,
    }


class Fault(Exception):
    """A named, bounded case failure."""

    def __init__(self, kind, detail):
        super().__init__(detail)
        self.kind = kind
        self.detail = detail


class Skip(Exception):
    """A case that a latched fault or dependency left unproven."""


class Infrastructure(Exception):
    """The environment cannot observe same-uid processes: no case is evidence.

    Raised by `preflight` before any case runs. It classifies the host, never the
    product: an unreadable same-uid process is exactly what custody must refuse,
    so the proof says `infrastructure`, unqualified, and stops before a case
    could mistake a capability limitation for a case result.
    """

    def __init__(self, check, detail, record=None):
        super().__init__(detail)
        self.check = check
        self.detail = detail
        self.record = record or {}


def preflight_outcome(probe, deleted, skipped, pending):
    """How the product's probe deletion ended, and what that classifies as.

    The census is the Barge binary's own custody scan, so its only positive
    result is the probe unit actually being deleted. A per-unit refusal whose
    reason is the product's `custody is not proven` — an unreadable same-uid
    process after its bounded re-observation, or a scan bound — is
    INFRASTRUCTURE. Any other outcome is an apparatus defect, because the probe
    unit is one Barge created, concluded and released itself.
    """
    if probe in deleted:
        return "safe", None
    if pending:
        return "defect", f"the probe deletion left {len(pending)} pending intent(s)"
    for row in skipped:
        if row.get("name") != probe:
            continue
        reason = row.get("reason") or "the probe unit was skipped without a reason"
        if reason.startswith("custody is not proven"):
            return "refused", reason
        return "defect", f"the probe unit was skipped for a non-custody reason: {reason}"
    return "defect", "the probe root was neither deleted nor skipped"


class Proof:
    def __init__(self, binary, receipt_path, diagnostic):
        self.binary = binary
        self.receipt_path = receipt_path
        self.diagnostic = diagnostic
        self.root = None
        self.state = None
        self.home = None
        self.mount = None
        self.image = None
        self.device = None
        self.whole_disk = None
        self.uuid = None
        self.results = []
        self.case_evidence = []
        self.fault = None
        self.preflight = None
        self.infrastructure = None
        self.real_state = None
        self.resolver = None
        self.attached = False
        self.env = None

    # -- fixture setup ----------------------------------------------------

    def setup(self):
        scratch = os.environ.get("TMPDIR")
        if not scratch or not os.path.isdir(scratch):
            raise Fault("premise", "TMPDIR is not a directory the guard created")
        # Premise 2's before-observation: the real account state as it is now,
        # observed through descriptors without following a link and only ever
        # complete if every entry and byte was read.
        self.real_state = {
            "path": os.path.join(account_home(), REAL_STATE_RELATIVE),
            "account_home": account_home(),
            "before": real_state_observation(),
            "after": None,
            "complete": False,
            "unchanged": None,
        }
        self.root = tempfile.mkdtemp(prefix="barge-scratch-proof-", dir=scratch)
        self.home = tempfile.mkdtemp(prefix="home-", dir=self.root)
        self.state = tempfile.mkdtemp(prefix="state-", dir=self.root)
        self.env = dict(os.environ)
        self.env.update(
            HOME=self.home,
            CARGO_HOME=os.path.join(self.home, "cargo"),
            BARGE_STATE_DIR=self.state,
            TMPDIR=self.root,
            TMP=self.root,
            TEMP=self.root,
        )
        os.makedirs(self.env["CARGO_HOME"], exist_ok=True)
        self.image = os.path.join(self.root, "scratch-volume.sparseimage")
        for name in ("hdiutil", "diskutil"):
            if shutil.which(name) is None:
                raise Fault("premise", f"{name} is not available")
        self.create_image()

    def create_image(self):
        run(["hdiutil", "create", *CREATE_IMAGE_OPTIONS, self.image], self.env)
        name = f"barge-proof-{os.getpid()}-{int(time.time()) % 100000}"
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
        self.register()

    def register(self):
        """Register the owned image's volume: the proof's own explicit fixture."""
        registry = {
            "schema": "barge.registry/1",
            "volumes": [
                {
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
            ],
        }
        with open(os.path.join(self.state, "registry.json"), "w", encoding="utf-8") as handle:
            json.dump(registry, handle, indent=1)
            handle.write("\n")

    def grants(self):
        with open(os.path.join(self.state, "registry.json"), encoding="utf-8") as handle:
            return json.load(handle)["volumes"][0]["scratch_grants"]

    # -- the CLI ----------------------------------------------------------

    def retain_evidence(self, case, document):
        """Retain one failure's decision evidence on the receipt."""
        evidence = expiry_evidence(document)
        self.case_evidence.append({"case": case, "at_ms": now_ms(), "evidence": evidence})
        return evidence

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

    def alloc(self, project, consumer, pid=None):
        code, document = self.barge(
            "scratch", "alloc", "--volume", self.uuid, "--project", project,
            "--consumer", consumer, "--pid", str(pid if pid is not None else os.getpid()),
            expect=0,
        )
        return document["data"]["root"]

    def conclude(self, root_id, holder, closure="positive", kept_reason="evidence",
                 nested_scope="exclusive", outcome="closed"):
        _code, document = self.barge(
            "scratch", "conclude", "--volume", self.uuid, "--root", root_id,
            "--holder", holder, "--outcome", outcome, "--closure", closure,
            "--kept-reason", kept_reason, "--nested-scope", nested_scope, expect=0,
        )
        return document["data"]

    def plan(self, expect=0):
        _code, document = self.barge("scratch", "expire", "--volume", self.uuid, "--plan", expect=expect)
        return document

    def expire(self, digest, expect=0):
        _code, document = self.barge(
            "scratch", "expire", "--volume", self.uuid, "--plan-sha256", digest, expect=expect
        )
        return document

    def release(self, root_id, holder):
        self.barge("area", "release", "--root", root_id, "--holder", holder, expect=0)

    # -- helpers ----------------------------------------------------------

    def scratch(self):
        return os.path.join(self.mount, "scratch")

    def project(self, name):
        path = os.path.join(self.scratch(), name)
        os.makedirs(path, exist_ok=True)
        return path

    def age(self, path, days):
        stamp = time.strftime("%Y%m%d%H%M", time.gmtime(time.time() - days * 86400))
        run(["find", path, "-exec", "touch", "-t", f"{stamp}.00", "{}", "+"], self.env)

    def marker_bytes(self, root_path):
        with open(os.path.join(root_path, ".barge-root.json"), "rb") as handle:
            return handle.read()

    def age_conclusion(self, root_path, days):
        marker = os.path.join(root_path, ".barge-root.json")
        with open(marker, encoding="utf-8") as handle:
            document = json.load(handle)
        document["conclusion"]["concluded_ms"] = now_ms() - days * 86400 * 1000
        mode = os.stat(marker).st_mode
        with open(marker, "w", encoding="utf-8") as handle:
            json.dump(document, handle, indent=2)
            handle.write("\n")
        os.chmod(marker, mode)

    def write_marker(self, path, document):
        target = os.path.join(path, document.pop("__name__"))
        with open(target, "w", encoding="utf-8") as handle:
            json.dump(document, handle)
        os.chmod(target, 0o600)

    def identity(self, path):
        info = os.lstat(path)
        return info.st_dev, info.st_ino

    def wait_for_quarantine(self, folder, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for name in os.listdir(folder) if os.path.isdir(folder) else []:
                if name.startswith(".barge-prune-"):
                    return os.path.join(folder, name)
            time.sleep(0.02)
        raise Fault("timing", "no quarantine leaf appeared before the deletion finished")

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

    def empty_plan_digest(self):
        document = self.plan()
        return document["data"]["plan_digest"], document["data"]["plan"]


    # -- cases ------------------------------------------------------------

    def case_two_concurrent_allocations_get_distinct_ids(self):
        project = "concurrent"
        # Both allocations are launched before either is awaited, so the case
        # proves concurrent distinct ids rather than two sequential allocations.
        commands = [
            [
                self.binary, "scratch", "alloc", "--volume", self.uuid,
                "--project", project, "--consumer", consumer,
                "--pid", str(os.getpid()), "--json",
            ]
            for consumer in ("first", "second")
        ]
        processes = [
            subprocess.Popen(
                command, env=self.env, cwd=self.root,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            for command in commands
        ]
        roots = []
        for process in processes:
            out, err = process.communicate(timeout=120)
            if process.returncode != 0:
                raise Fault(
                    "case",
                    f"an overlapping allocation exited {process.returncode}: "
                    f"{(err or out).strip()[-300:]}",
                )
            try:
                roots.append(json.loads(out)["data"]["root"])
            except (ValueError, KeyError) as error:
                raise Fault("case", f"an allocation printed no root: {error}: {out[-300:]}")
        first, second = roots
        if first["root_id"] == second["root_id"]:
            raise Fault("case", "two allocations received the same root id")
        for root in (first, second):
            if not os.path.isdir(root["path"]):
                raise Fault("case", f"{root['path']} is not a directory")
            mode = os.stat(root["path"]).st_mode & 0o777
            if mode != 0o700:
                raise Fault("case", f"the root's mode is {mode:o}, not 700")
        self.held = {"first": first, "second": second}
        return f"distinct root ids {first['root_id'][:8]}, {second['root_id'][:8]}"

    def case_one_root_concluded_one_left_unconcluded(self):
        root = self.held["first"]
        self.conclude(root["root_id"], "first")
        self.release(root["root_id"], "first")
        unconcluded = self.held["second"]
        marker = json.loads(self.marker_bytes(unconcluded["path"]))
        if marker["conclusion"] is not None:
            raise Fault("case", "the unconcluded root recorded a conclusion")
        if json.loads(self.marker_bytes(root["path"]))["conclusion"]["closure"] != "positive":
            raise Fault("case", "the concluded root did not record a positive closure")
        return "one positive conclusion, one unconcluded root"

    def case_a_synthetic_old_conclusion_expires(self):
        root = self.held["first"]
        self.age_conclusion(root["path"], NATIVE_AGE_DAYS + 1)
        document = self.plan()
        candidates = [row["name"] for row in document["data"]["plan"]["candidates"]["rows"]]
        if candidates != [root["root_id"]]:
            raise Fault("case", f"the plan's candidates are {candidates}, not the one old root")
        digest = document["data"]["plan_digest"]
        real = self.expire(digest)
        if real["data"]["deleted"]["count"] != 1:
            evidence = self.retain_evidence("a_synthetic_old_conclusion_expires", real)
            raise Fault(
                "case",
                f"the expiry deleted {real['data']['deleted']['count']} roots; "
                f"evidence {json.dumps(evidence, sort_keys=True)[:1500]}",
            )
        if os.path.exists(root["path"]):
            raise Fault("case", "the expired root is still there")
        unconcluded = self.held["second"]["path"]
        if not os.path.isdir(unconcluded):
            raise Fault("case", "the unconcluded root did not survive")
        if real["data"]["deleted"]["rows"][0]["marker_removed"] is not True:
            raise Fault("case", "the marker was not unlinked last")
        return "the old conclusion expired; the unconcluded root survived; the marker went last"

    def case_an_unknown_entry_stops_the_delete(self):
        root = self.alloc("unknown-entry", "owner")
        self.conclude(root["root_id"], "owner")
        self.release(root["root_id"], "owner")
        with open(os.path.join(root["path"], "payload.txt"), "w", encoding="utf-8") as handle:
            handle.write("payload")
        with open(os.path.join(root["path"], ".DS_Store"), "w", encoding="utf-8") as handle:
            handle.write("finder")
        os.symlink("/elsewhere", os.path.join(root["path"], "unknown-link"))
        self.age_conclusion(root["path"], NATIVE_AGE_DAYS + 1)
        document = self.plan()
        if document["data"]["plan"]["candidates"]["count"] != 1:
            raise Fault("case", "the root with an unknown entry is not a candidate")
        real = self.expire(document["data"]["plan_digest"], expect=3)
        if real["data"]["pending"]["count"] != 1:
            raise Fault("case", "the interrupted delete left no pending intent")
        phase = real["data"]["pending"]["rows"][0]["phase"]
        if phase != "renamed":
            raise Fault("case", f"the pending intent is at `{phase}`, not `renamed`")
        leaves = [name for name in os.listdir(self.project("unknown-entry"))
                  if name.startswith(".barge-prune-")]
        if len(leaves) != 1:
            raise Fault("case", "the quarantine leaf was not kept")
        payload = os.path.join(self.project("unknown-entry"), leaves[0], "payload.txt")
        if not os.path.isfile(payload):
            raise Fault("case", "the stopped delete removed the payload")
        self.held["unknown"] = {"root_id": root["root_id"], "path": root["path"],
                                "leaf": os.path.join(self.project("unknown-entry"), leaves[0])}
        return "an unknown entry stopped the delete at `renamed`; the root survived as the leaf"

    def case_a_clean_root_is_deleted_and_the_marker_goes_last(self):
        # First settle the pending intent the previous case left: a real run
        # replays it before its plan's effects, and a plan with no candidates
        # keeps this step to the replay.
        self.settle_pending()
        root = self.alloc("clean", "owner")
        self.conclude(root["root_id"], "owner")
        self.release(root["root_id"], "owner")
        for index in range(12):
            folder = os.path.join(root["path"], f"nested-{index}")
            os.makedirs(folder)
            with open(os.path.join(folder, "payload.txt"), "w", encoding="utf-8") as handle:
                handle.write("payload")
        with open(os.path.join(root["path"], ".DS_Store"), "w", encoding="utf-8") as handle:
            handle.write("finder")
        self.age_conclusion(root["path"], NATIVE_AGE_DAYS + 1)
        document = self.plan()
        real = self.expire(document["data"]["plan_digest"])
        deleted = real["data"]["deleted"]["rows"]
        if len(deleted) != 1 or deleted[0]["name"] != root["root_id"]:
            raise Fault("case", f"the clean root was not the only deletion: {deleted}")
        if deleted[0]["marker_removed"] is not True:
            raise Fault("case", "the marker was not unlinked last")
        if deleted[0]["entries_removed"] < 13:
            raise Fault("case", f"only {deleted[0]['entries_removed']} entries were removed")
        if os.path.exists(root["path"]):
            raise Fault("case", "the clean root is still there")
        return f"deleted whole: {deleted[0]['entries_removed']} entries, marker last"

    def case_an_injected_enotempty_leaves_a_pending_intent(self):
        root = self.alloc("enotempty", "owner")
        self.conclude(root["root_id"], "owner")
        self.release(root["root_id"], "owner")
        for index in range(200):
            with open(os.path.join(root["path"], f"file-{index}.txt"), "w", encoding="utf-8") as handle:
                handle.write("payload")
        self.age_conclusion(root["path"], NATIVE_AGE_DAYS + 1)
        document = self.plan()
        digest = document["data"]["plan_digest"]
        folder = self.project("enotempty")
        command = [
            self.binary, "scratch", "expire", "--volume", self.uuid,
            "--plan-sha256", digest, "--json",
        ]
        process = subprocess.Popen(
            command, env=self.env, cwd=self.root,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        # A writer keeps recreating an entry inside the quarantined leaf while the
        # deletion runs, so the delete meets an entry it did not classify (or a
        # leaf that is not empty when it is removed). Both are the conservative
        # stop the design names: the intent stays pending and the leaf is kept.
        # The marker-last `ENOTEMPTY` retry itself is proved by the counted unit
        # counterfactual (`delete::tests::enotempty_three_times_...`); a native
        # cannot inject a platform fault at an exact step.
        planted_rounds = 0
        try:
            leaf = self.wait_for_quarantine(folder, timeout=60)
            planted = os.path.join(leaf, ".DS_Store")
            deadline = time.time() + 30
            while time.time() < deadline:
                try:
                    os.makedirs(planted, exist_ok=True)
                    planted_rounds += 1
                except OSError:
                    pass
                if not os.path.isdir(leaf):
                    break
                time.sleep(0.01)
        finally:
            process.wait(timeout=180)
        document = json.loads(process.stdout.read() or "{}")
        if process.returncode not in (0, 3):
            raise Fault("case", f"the expiry exited {process.returncode}: {document}")
        pending = self.pending_intents()
        if len(pending) != 1:
            raise Fault(
                "case",
                f"expected one pending intent after the plant, found {len(pending)} "
                f"(exit {process.returncode}, planted {planted_rounds} times)",
            )
        phase = pending[0]["phase"]
        if phase not in ("renamed", "marker_last"):
            raise Fault("case", f"the pending intent is at `{phase}`")
        leaf = os.path.join(folder, pending[0]["unit"]["capture_name"])
        if not os.path.isdir(leaf):
            raise Fault("case", "the quarantine leaf was not kept")
        reason = document.get("data", {}).get("pending", {}).get("rows", [{}])[0].get("reason")
        self.settle_pending()
        return (
            f"a mid-delete entry stopped the delete at `{phase}` ({reason}); "
            "the leaf was kept and the replay settled it"
        )

    def case_a_pull_mid_delete_leaves_a_pending_intent_then_replay(self):
        root = self.alloc("pull", "owner")
        self.conclude(root["root_id"], "owner")
        self.release(root["root_id"], "owner")
        for folder_index in range(80):
            folder = os.path.join(root["path"], f"bulk-{folder_index:02}")
            os.makedirs(folder)
            for index in range(100):
                with open(os.path.join(folder, f"file-{index:03}.txt"), "w", encoding="utf-8") as handle:
                    handle.write("payload")
        self.age_conclusion(root["path"], NATIVE_AGE_DAYS + 1)
        document = self.plan()
        digest = document["data"]["plan_digest"]
        folder = self.project("pull")
        command = [
            self.binary, "scratch", "expire", "--volume", self.uuid,
            "--plan-sha256", digest, "--json",
        ]
        process = subprocess.Popen(
            command, env=self.env, cwd=self.root,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        pulled = None
        try:
            self.wait_for_quarantine(folder, timeout=60)
            pulled = self.pull()
        finally:
            try:
                process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                process.kill()
                raise Fault("case", "the expiry did not stop after the volume was pulled")
        # The pending intent is state, not the image: it must be readable while the
        # volume is away.
        pending = self.pending_intents()
        if len(pending) != 1:
            raise Fault("case", f"expected one pending intent after the pull, found {len(pending)}")
        intent = pending[0]
        kept_name = intent["unit"]["capture_name"]
        self.reattach()
        # Only now, after re-attach, is the on-image identity checkable: the kept
        # quarantine leaf, its marker, and the device and inode the intent recorded.
        kept = os.path.join(folder, kept_name)
        if not os.path.isdir(kept):
            raise Fault("case", "the quarantine leaf was not kept through the pull")
        if not os.path.isfile(os.path.join(kept, ".barge-root.json")):
            raise Fault("case", "the kept leaf lost its marker")
        device, inode = self.identity(kept)
        if (device, inode) != (intent["unit"]["device"], intent["unit"]["inode"]):
            raise Fault(
                "case",
                f"the kept leaf's identity is {device}:{inode}, not the intent's "
                f"{intent['unit']['device']}:{intent['unit']['inode']}",
            )
        digest_after, plan = self.empty_plan_digest()
        if plan["candidates"]["count"] != 0:
            raise Fault("case", "a settled-plan run would delete something new")
        real = self.expire(digest_after, expect=0)
        replay = real["data"].get("replay") or {}
        if not replay.get("resumed") and not replay.get("settled"):
            raise Fault("case", f"the replay did not resume or settle the intent: {replay}")
        if self.pending_intents():
            raise Fault("case", "the intent is still pending after the replay")
        if os.path.isdir(kept):
            raise Fault("case", "the kept leaf survived the replay")
        return (
            f"a {pulled} pull mid-delete kept the leaf; the identity recorded in the "
            "intent survived the re-attach, and the replay settled it"
        )

    def case_a_stale_mount_point_is_never_written(self):
        failure = self.detach()
        if failure is not None:
            raise Fault("case", failure)
        stale = self.mount
        existed = os.path.isdir(stale)
        code, document = self.barge("scratch", "expire", "--volume", self.uuid, "--plan", expect=2)
        if document["schema"] != "barge.error/1" or document["error"]["kind"] != "volume_absent":
            raise Fault("case", f"an absent permanent volume did not refuse: {document}")
        if existed and os.path.isdir(os.path.join(stale, "scratch")):
            raise Fault("case", "the stale mount point was written to")
        if os.path.isdir(stale) and not existed:
            raise Fault("case", "the stale mount point was recreated")
        self.reattach()
        return "an absent volume refused and the stale path was neither written nor recreated"

    def case_foreign_and_unmarked_units_go_only_under_grants(self):
        oxide_root = os.path.join(self.project("oxide"), "oxide-proof.native")
        os.makedirs(oxide_root)
        device, inode = self.identity(oxide_root)
        self.write_marker(oxide_root, {
            "__name__": ".oxide-scratch.json",
            "version": 1,
            "run_id": "0123456789abcdef0123456789abcdef",
            "root": oxide_root,
            "owner_uid": os.geteuid(),
            "device": device,
            "inode": inode,
            "created_unix": time.time() - 30 * 86400,
            "concluded_unix": time.time() - 30 * 86400,
            "outcome": "closed",
            "closure": "positive",
            "kept_reason": "evidence",
            "nested_scope": "exclusive",
        })
        effigy_root = os.path.join(self.scratch(), "dead-run")
        os.makedirs(effigy_root)
        self.write_marker(effigy_root, {
            "__name__": ".effigy-scratch.json",
            "run": "dead-run",
            "pid": 4294967294,
            "pid_start": "1:2",
            "created_ms": now_ms() - 30 * 86400 * 1000,
            "keep": True,
            "concluded_ms": now_ms() - 30 * 86400 * 1000,
        })
        unmarked = os.path.join(self.project("granted"), "leftover")
        os.makedirs(unmarked)
        with open(os.path.join(unmarked, "payload.txt"), "w", encoding="utf-8") as handle:
            handle.write("payload")
        unclaimed = os.path.join(self.project("ungranted"), "leftover")
        os.makedirs(unclaimed)
        self.age(self.scratch(), 30)

        document = self.plan()
        if document["data"]["plan"]["candidates"]["count"] != 0:
            raise Fault("case", "a unit was a candidate without a grant")
        kept = [row["name"] for row in document["data"]["plan"]["kept"]["rows"]]
        for name in ("oxide-proof.native", "dead-run", "leftover"):
            if name not in kept:
                raise Fault("case", f"`{name}` is not reported kept without a grant")

        self.barge("scratch", "delegate", "--volume", self.uuid,
                   "--family", "oxide_scratch", "--by", "operator", expect=0)
        self.barge("scratch", "delegate", "--volume", self.uuid,
                   "--family", "effigy_scratch", "--by", "operator", expect=0)
        self.barge("scratch", "claim", "--volume", self.uuid, "--scope", "project:granted",
                   "--unit", "child", "--idle-seconds", str(CLAIM_IDLE_SECONDS),
                   "--exclude-family", "oxide_scratch", "--exclude-family", "effigy_scratch",
                   "--declared-by", "operator", expect=0)
        granted = self.grants()
        if len(granted["delegations"]) != 2 or len(granted["claims"]) != 1:
            raise Fault("case", f"the grants recorded are {granted}")

        document = self.plan()
        candidates = sorted(row["name"] for row in document["data"]["plan"]["candidates"]["rows"])
        if candidates != sorted(["oxide-proof.native", "dead-run", "leftover"]):
            raise Fault("case", f"the granted candidates are {candidates}")
        real = self.expire(document["data"]["plan_digest"])
        if real["data"]["deleted"]["count"] != 3:
            raise Fault("case", f"the granted sweep deleted {real['data']['deleted']['count']}")
        for gone in (oxide_root, effigy_root, unmarked):
            if os.path.exists(gone):
                raise Fault("case", f"{gone} survived the granted sweep")
        if not os.path.isdir(unclaimed):
            raise Fault("case", "the ungranted scope's unit was deleted")
        return "all three survived without grants; only the eligible ones went once delegated and claimed"

    # -- pending-intent housekeeping -------------------------------------

    def settle_pending(self):
        # An operator resolves what the stopped delete refused before replaying:
        # only the obstacle a case planted is removed from the kept quarantine
        # leaf (a symlink, or the metadata-shaped plant), and the replay then
        # finishes the rest from the recorded identity alone. Nothing else is
        # touched here: the payloads go through Barge's own delete.
        for intent in self.pending_intents():
            folder = os.path.join(self.mount, intent["root_dir"], intent["folder"], intent["unit"]["parent"])
            leaf = os.path.join(folder, intent["unit"]["capture_name"])
            if not os.path.isdir(leaf):
                continue
            for name in os.listdir(leaf):
                if name == ".barge-root.json":
                    continue
                target = os.path.join(leaf, name)
                if os.path.islink(target):
                    os.unlink(target)
                elif os.path.isdir(target) and name == ".DS_Store":
                    shutil.rmtree(target)
        digest, plan = self.empty_plan_digest()
        if plan["candidates"]["count"] != 0:
            raise Fault("case", "a settle run would delete more than the pending intent")
        real = self.expire(digest, expect=0)
        replay = real["data"].get("replay") or {}
        if replay.get("pending"):
            raise Fault("case", f"the replay left intents pending: {replay['pending']}")
        if self.pending_intents():
            raise Fault("case", "the pending intent did not settle")

    # -- attach and detach ------------------------------------------------

    def reattach(self):
        if self.attached:
            return
        run(
            ["hdiutil", "attach", self.image, "-mountpoint", self.mount, *ATTACH_OPTIONS],
            self.env,
            timeout=180,
        )
        self.attached = True

    def detach(self):
        """Detach the owned image ordinarily, and never by force.

        Premise 5 allows a forced detach only of the owned image's own whole-disk
        device, and only the pull case uses it. A failed ordinary detach here
        leaves the image attached, so the caller can keep its backing and report
        unqualified instead of force-unmounting a path.
        """
        if not self.attached:
            return None
        result = run(["hdiutil", "detach", self.mount], self.env, timeout=180, check=False)
        if result.returncode != 0:
            return (
                f"the ordinary detach of {self.mount} failed "
                f"({result.returncode}: {(result.stderr or result.stdout).strip()[-200:]})"
            )
        self.attached = False
        return None

    def pull(self):
        """Pull the owned image mid-delete with the ordinary detach the case names.

        The brief's case is an ordinary detach mid-delete. A forced detach is
        never substituted for it: if the ordinary detach is refused, the case is
        unproved and says so, rather than proving a different behaviour under the
        same name.
        """
        result = run(["hdiutil", "detach", self.mount], self.env, timeout=120, check=False)
        if result.returncode != 0:
            raise Fault(
                "case",
                "the ordinary detach mid-delete was refused "
                f"({result.returncode}: {(result.stderr or result.stdout).strip()[-200:]}), "
                "so this case is unproved; no forced detach is used as a substitute",
            )
        self.attached = False
        return "ordinary"

    def assert_resolver(self):
        """Prove the product's state resolution from fixture-only evidence.

        `describe` is not used: it observes live Oxide pools and removable
        records, which a proof must never read. Instead a state-only verb reads
        and writes only the state directory: the delegation it records has to land
        in this proof's fixture registry (a resolver that had chosen the real
        account state could not see that registry, would refuse `unknown_state`,
        and would write there instead), and the withdrawal then has to remove it
        from the same fixture registry. The account home is the password
        database's, not this proof's `HOME`, so an override is the only thing that
        could have moved state, and premise 2's before/after observation covers
        the real directory.
        """
        home = account_home()
        # The evidence is the environment the *child* commands run with, not this
        # process's: the guard's environment is not what the product reads.
        problems = child_environment_problems(self.env, self.state, self.home, home)
        if problems:
            raise Fault("premise", "; ".join(problems))
        registry_path = os.path.join(self.state, "registry.json")
        if not os.path.isfile(registry_path):
            raise Fault("premise", "the fixture state directory holds no registry")
        resolver = "barge-proof-resolver"
        self.barge(
            "scratch", "delegate", "--volume", self.uuid, "--family",
            "effigy_scratch", "--by", resolver, expect=0,
        )
        with open(registry_path, encoding="utf-8") as handle:
            recorded = json.load(handle)["volumes"][0]["scratch_grants"]["delegations"]
        if [row["by"] for row in recorded] != [resolver]:
            raise Fault(
                "premise",
                f"the recorded delegation did not land in the fixture registry: {recorded}",
            )
        self.barge(
            "scratch", "delegate", "--volume", self.uuid, "--family",
            "effigy_scratch", "--by", resolver, "--revoke", expect=0,
        )
        with open(registry_path, encoding="utf-8") as handle:
            withdrawn = json.load(handle)["volumes"][0]["scratch_grants"]["delegations"]
        if withdrawn:
            raise Fault(
                "premise",
                f"the withdrawn delegation is still recorded in the fixture: {withdrawn}",
            )
        self.resolver = {
            "command": "scratch.delegate/scratch.revoke",
            "environment": "the child commands' own environment, not this process's",
            "state_dir": self.state,
            "source": "the child's BARGE_STATE_DIR override (the only path that moves state)",
            "account_home": home,
            "fixture_home": self.home,
            "observed": "the delegation was recorded in and withdrawn from the fixture registry",
        }

    def product_census(self):
        """Prove the product's same-uid census from a real deletion, before any case.

        The proof does not observe the host itself: the Barge binary does. This
        allocates one probe root in the guard's owned image, concludes it
        positively, releases its owner hold and ages the conclusion past the
        native policy, then has a real `barge scratch expire` delete it. Custody
        runs the product's own same-effective-uid descriptor scan — the same
        observer, bounds, foreign-uid classification, exited-process handling and
        bounded EPERM re-observation every case's deletion uses — so the verdict
        is the census, decided by the binary the cases run.

        A product custody refusal (an unreadable same-uid process after its
        bounded re-observation, or a scan bound) is INFRASTRUCTURE: unqualified,
        aborted before any case, with the product's own reason in the receipt. Any
        other outcome would be an apparatus defect, because the proof created and
        concluded the probe unit itself.
        """
        root = self.alloc(PREFLIGHT_PROJECT, PREFLIGHT_CONSUMER)
        self.conclude(root["root_id"], PREFLIGHT_CONSUMER)
        self.release(root["root_id"], PREFLIGHT_CONSUMER)
        self.age_conclusion(root["path"], NATIVE_AGE_DAYS + 1)
        document = self.plan()
        candidates = [row["name"] for row in document["data"]["plan"]["candidates"]["rows"]]
        if candidates != [root["root_id"]]:
            raise Fault(
                "premise",
                f"the probe root is not the plan's only candidate: {candidates}",
            )
        digest = document["data"]["plan_digest"]
        real = self.expire(digest)
        deleted = [row["name"] for row in real["data"]["deleted"]["rows"]]
        skipped = real["data"]["skipped"]["rows"]
        pending = real["data"]["pending"]["rows"]
        outcome, reason = preflight_outcome(root["root_id"], deleted, skipped, pending)
        record = {
            "mode": "product-custody-census",
            "observer": "the barge binary's own custody scan, the binary the cases run",
            "bounds": PRODUCT_CUSTODY_BOUNDS,
            "probe_root": root["root_id"],
            "plan_digest": digest,
            "outcome": outcome,
            "reason": reason,
            "deleted": len(deleted),
            "skipped": len(skipped),
            "pending": len(pending),
        }
        self.preflight = record
        if outcome == "safe":
            return record
        if outcome == "defect":
            raise Fault("premise", reason)
        raise Infrastructure("product_custody_census", reason, record)

    # -- the run ----------------------------------------------------------

    def run_cases(self):
        passed = set()
        for name, needs in CASES:
            missing = [need for need in needs if need not in passed]
            skip = Skip(f"a dependency did not pass: {', '.join(missing)}") if missing else None
            if skip is None and self.fault is not None:
                skip = Skip("a diagnostic fault is latched; all remaining cases depend on the failed case in the linear chain")
            if skip is not None:
                self.results.append({"case": name, "status": "skipped", "detail": str(skip)})
                continue
            started = time.time()
            try:
                detail = getattr(self, f"case_{name}")()
            except Skip as error:
                self.results.append({"case": name, "status": "skipped", "detail": str(error)})
            except Fault as error:
                self.results.append({
                    "case": name, "status": "failed", "kind": error.kind,
                    "detail": error.detail, "ms": int((time.time() - started) * 1000),
                })
                if not self.diagnostic:
                    raise
                self.fault = name
            except Exception as error:  # a case bug is a fault, never a pass
                self.results.append({
                    "case": name, "status": "failed", "kind": "exception",
                    "detail": f"{type(error).__name__}: {error}",
                    "trace": traceback.format_exc()[-800:],
                })
                if not self.diagnostic:
                    raise
                self.fault = name
            else:
                passed.add(name)
                self.results.append({
                    "case": name, "status": "passed", "detail": detail,
                    "ms": int((time.time() - started) * 1000),
                })

    def receipt(self, cancelled=None):
        # A proof is qualified only when the host was observable, every case ran
        # and passed, and nothing was cancelled. An infrastructure abort is
        # unqualified in both modes and carries no case rows at all: no case ran.
        classification = "infrastructure" if self.infrastructure else "proof"
        state_ok = bool(
            self.real_state
            and self.real_state.get("complete") is True
            and self.real_state.get("unchanged") is True
        )
        return {
            "schema": SCHEMA,
            "qualified": (classification == "proof"
                          and not self.diagnostic
                          and self.fault is None
                          and cancelled is None
                          and state_ok
                          and self.resolver is not None
                          and all(row["status"] == "passed" for row in self.results)
                          and len(self.results) == len(CASES)),
            "classification": classification,
            "infrastructure": self.infrastructure,
            "preflight": self.preflight,
            "real_state": self.real_state,
            "resolver": self.resolver,
            "diagnostic": self.diagnostic,
            "cancelled": cancelled,
            "cleanup_failure": cancelled,
            "wall_limit_ms": WALL_MS,
            "scratch_budget_mib": SCRATCH_MIB,
            "image_mib": IMAGE_MIB,
            "cases": self.results,
            "case_evidence": self.case_evidence,
            "premises": PREMISES,
            "environment": sorted(ENVIRONMENT),
            "environment_purposes": dict(ENVIRONMENT),
            "supervisor_sha256": SUPERVISOR_SHA256,
            "bin_sha256": sha256_file(self.binary) if os.path.isfile(self.binary) else None,
            "owned_scratch": self.root,
            "at_ms": now_ms(),
        }

    def write_receipt(self, receipt):
        if not self.receipt_path:
            return
        os.makedirs(os.path.dirname(self.receipt_path), exist_ok=True)
        with open(self.receipt_path, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=1)
            handle.write("\n")

    def cleanup(self):
        """Detach ordinarily and remove the owned root; a failure keeps both.

        A failed ordinary detach leaves the image attached and its backing file in
        place: the proof never force-detaches by mount path and never removes the
        backing of an image it could not detach. The failure is returned so the
        receipt is unqualified, never suppressed.
        """
        failure = self.detach()
        if failure is not None:
            return failure
        if self.root and os.path.isdir(self.root):
            shutil.rmtree(self.root, ignore_errors=True)
        return None


def expiry_evidence(document):
    """The decision evidence of one `barge scratch expire` document.

    A case that fails on a real expiry must say *why* the sweep kept its
    candidate: the plan it recomputed, what it skipped and any pending intent.
    The projection is fixed and small, so it can be carried in a case detail and
    retained in the diagnostic receipt without widening the receipt bound.
    """
    data = document.get("data") or {}
    plan = data.get("plan") or {}

    def rows(block):
        return list((block or {}).get("rows") or [])

    return {
        "schema": document.get("schema"),
        "ok": document.get("ok"),
        "error": document.get("error"),
        "plan_digest": data.get("plan_digest"),
        "volume_present": data.get("volume_present"),
        "preview": data.get("preview"),
        "candidates": [
            {
                "name": row.get("name"),
                "scope": row.get("scope"),
                "family": row.get("family"),
                "grant": row.get("grant"),
                "reason": row.get("reason"),
                "marker_digest": row.get("marker_digest"),
            }
            for row in rows(plan.get("candidates"))
        ],
        "kept_count": (plan.get("kept") or {}).get("count"),
        "examined": plan.get("examined"),
        "truncated": plan.get("truncated"),
        "deleted": rows(data.get("deleted")),
        "pending": rows(data.get("pending")),
        "skipped": rows(data.get("skipped")),
        "replay": data.get("replay"),
    }


def preflight_problems():
    """Pure tests of the product-census preflight, its classification and limits.

    The proof does not observe the host: the Barge binary's own custody scan is
    the census, exercised through a real probe deletion. So the checks here are
    the classifier's outcomes on the product's own report, the guarantee that no
    Python process observation remains, and the source of the probe itself.
    """
    problems = []
    probe = "0123456789abcdef0123456789abcdef"
    custody = "custody is not proven: the open-descriptor scan is unreadable: pid 7 (dmd)"

    # The only positive result is the probe unit actually deleted.
    if preflight_outcome(probe, [probe], [], []) != ("safe", None):
        problems.append("a deleted probe root is not a safe census")
    if preflight_outcome(probe, [], [{"name": "other", "reason": "x"}], [])[0] != "defect":
        problems.append("a skip of another unit was read as the census")

    # A custody refusal is infrastructure, with the product's own reason.
    outcome, reason = preflight_outcome(probe, [], [{"name": probe, "reason": custody}], [])
    if outcome != "refused":
        problems.append("a product custody refusal is not infrastructure")
    elif reason != custody:
        problems.append("the product's custody reason is not carried through")

    # Anything else is an apparatus defect, not infrastructure and not a pass.
    for skipped, pending, why in (
        ([{"name": probe, "reason": "the unit is no longer eligible"}], [],
         "a non-custody skip"),
        ([], [{"intent_id": "ab"}], "a pending probe deletion"),
        ([], [], "a probe root that was neither deleted nor skipped"),
    ):
        outcome, _reason = preflight_outcome(probe, [], skipped, pending)
        if outcome != "defect":
            problems.append(f"{why} was not an apparatus defect")

    # No Python-side process observation may remain: the census is the binary's.
    # The check's own text names what it forbids, so only the apparatus before it
    # is inspected.
    source = open(__file__, encoding="utf-8").read().split("def preflight_problems", 1)[0]
    for forbidden in ("proc_pidinfo", "KERN_PROC_ALL", "libproc", "ctypes"):
        if forbidden in source:
            problems.append(f"the proof still observes the host itself (`{forbidden}`)")
    census_source = inspect.getsource(Proof.product_census)
    for hook in ("scratch", "alloc", "conclude", "expire", "PREFLIGHT_PROJECT"):
        if hook not in census_source:
            problems.append(f"the census probe does not use `{hook}`")
    if "custody is not proven" in census_source:
        problems.append("the census matches the product's reason by hand, not in the classifier")
    if "preflight_outcome(" not in census_source:
        problems.append("the census does not classify the product's report")
    if "Infrastructure(" not in census_source:
        problems.append("a refused census does not classify as infrastructure")
    if PREFLIGHT_CHECKS != ["product_custody_census"]:
        problems.append("the preflight check names changed")
    if "4096" not in PRODUCT_CUSTODY_BOUNDS or "10 s" not in PRODUCT_CUSTODY_BOUNDS:
        problems.append("the recorded product bounds are not the S5 design's")
    return problems


def environment_hooks(source):
    """Find explicitly accessed or assigned environment keys in one proof source.

    The proof uses os.environ and the child environment dictionaries `env` and
    `self.env`. Reject computed keys and dynamic updates so they cannot silently
    escape the inventory. Copying the inherited environment itself does not name
    a hook; individual accesses and child overrides do.
    """
    hooks = set()
    problems = []

    def environment(node):
        return (isinstance(node, ast.Name) and node.id == "env") or (
            isinstance(node, ast.Attribute) and (
                (isinstance(node.value, ast.Name) and node.value.id == "os" and node.attr == "environ")
                or (isinstance(node.value, ast.Name) and node.value.id == "self" and node.attr == "env")
            )
        )

    def add_key(node):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            hooks.add(node.value)
        else:
            problems.append("an environment key is not a literal")

    for node in ast.walk(ast.parse(source)):
        if (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                and isinstance(node.func.value, ast.Name) and node.func.value.id == "os"
                and node.func.attr == "getenv"):
            if node.args:
                add_key(node.args[0])
            else:
                problems.append("an environment read has no key")
        elif isinstance(node, ast.Subscript) and environment(node.value):
            add_key(node.slice)
        elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
              and environment(node.func.value)):
            if node.func.attr == "get" and node.args:
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


def self_check():
    """The proof's own apparatus: the case map and its dispatch, pins, premises.

    The dispatch check is the guard that was missing when eight of nine case
    names had no method: `run_cases` calls `case_<name>`, so the self-check
    proves every case name resolves to a method and every case method is in the
    map, and that each dependency is evaluated before the case that needs it.
    """
    problems = preflight_problems()
    names = [name for name, _ in CASES]
    if len(names) != 9 or len(set(names)) != 9:
        problems.append("the case map does not name nine distinct cases")
    known = set(names)
    seen = set()
    for index, (name, needs) in enumerate(CASES):
        expected_needs = [] if index == 0 else [CASES[index - 1][0]]
        if needs != expected_needs:
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
    if not SUPERVISOR_SHA256 or len(SUPERVISOR_SHA256) != 64:
        problems.append("the supervisor digest is not a SHA-256")
    # The owned image's invocations: `hdiutil create` rejects `-owners`, and the
    # attach must enable ownership. Both are asserted on the option lists the
    # proof actually runs, not on a copy of them.
    if "-owners" in CREATE_IMAGE_OPTIONS:
        problems.append("`hdiutil create` is given `-owners`, which it does not accept")
    for flag, value in (("-fs", "APFS"), ("-type", "SPARSE")):
        if CREATE_IMAGE_OPTIONS.count(flag) != 1 or CREATE_IMAGE_OPTIONS.count(value) != 1:
            problems.append(f"the create invocation no longer declares {flag} {value}")
        else:
            index = CREATE_IMAGE_OPTIONS.index(flag)
            if CREATE_IMAGE_OPTIONS[index + 1] != value:
                problems.append(f"the create invocation's {flag} is not {value}")
    if f"{IMAGE_MIB}m" not in CREATE_IMAGE_OPTIONS or IMAGE_MIB > 128:
        problems.append("the create invocation's size is not the bounded 128 MiB")
    if CREATE_IMAGE_OPTIONS[-2:] != ["-volname", "barge-proof-scratch"]:
        problems.append("the create invocation's volume name changed")
    if ATTACH_OPTIONS.count("-owners") != 1:
        problems.append("the attach does not enable ownership exactly once")
    else:
        index = ATTACH_OPTIONS.index("-owners")
        if index + 1 >= len(ATTACH_OPTIONS) or ATTACH_OPTIONS[index + 1] != "on":
            problems.append("the attach's `-owners` is not followed by `on`")
    synthetic = {
        "schema": "barge.scratch.expire/1",
        "ok": True,
        "data": {
            "plan_digest": "ab" * 32,
            "volume_present": True,
            "preview": False,
            "plan": {
                "candidates": {
                    "count": 1,
                    "rows": [{"name": "c1", "scope": {"kind": "project", "name": "p"},
                              "family": "native", "grant": "native_root",
                              "reason": "native root", "marker_digest": "cd" * 32}],
                },
                "kept": {"count": 0, "rows": []},
                "examined": 3,
                "truncated": False,
            },
            "deleted": {"count": 0, "rows": []},
            "pending": {"count": 0, "rows": []},
            "skipped": {"count": 1, "rows": [{"name": "c1", "scope": {"kind": "project", "name": "p"},
                                               "reason": "custody is not proven: a reason"}]},
        },
    }
    projected = expiry_evidence(synthetic)
    for key in ("plan_digest", "candidates", "skipped", "pending", "deleted", "replay"):
        if key not in projected:
            problems.append(f"the expiry evidence projection omits {key}")
    if not projected["skipped"] or not projected["skipped"][0].get("reason"):
        problems.append("the expiry evidence projection drops the skip reason")
    # A document with no data (a refusal before any report) must project too, so
    # the failure path can never raise while reporting a failure.
    empty = expiry_evidence({})
    if empty.get("candidates") != [] or empty.get("skipped") != []:
        problems.append("the expiry evidence projection does not tolerate an empty document")
    if len(PREMISES) != 9:
        problems.append("the premise record does not name nine premises")
    script_dir = os.path.dirname(os.path.abspath(__file__))
    sources = []
    for name in ("test-scratch-expiry.py", "run-scratch-expiry-native.py"):
        with open(os.path.join(script_dir, name), encoding="utf-8") as source_file:
            sources.append(source_file.read())
    problems.extend(environment_inventory_problems(sources, ENVIRONMENT))
    # Counterfactuals: missing, extra and dynamically named hooks must all refuse.
    sample = 'os.environ.get("EXAMPLE")'
    if not environment_inventory_problems([sample], {}):
        problems.append("an omitted environment hook is not detected")
    if not environment_inventory_problems([sample], {"EXAMPLE": "purpose", "EXTRA": "purpose"}):
        problems.append("an unused environment hook is not detected")
    if not environment_inventory_problems(['os.environ.get(variable)'], {}):
        problems.append("a dynamic environment hook is not detected")
    if not environment_inventory_problems(['os.getenv("MISSING")'], {}):
        problems.append("an omitted os.getenv hook is not detected")
    return problems


def main(argv):
    parser = argparse.ArgumentParser(description="Barge's scratch expiry and deletion native proof")
    parser.add_argument("--diagnostic", action="store_true")
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--receipt", default=os.environ.get(
        "OXIDE_PROOF_EVIDENCE", os.environ.get("TMPDIR", ".")))
    options = parser.parse_args(argv)

    problems = self_check()
    if options.self_check:
        print(json.dumps({
            "schema": "barge.scratch.expiry.native.self-check/1",
            "cases": [name for name, _ in CASES],
            "dispatch": {name: f"case_{name}" for name, _ in CASES},
            "image": {
                "create": ["hdiutil", "create", *CREATE_IMAGE_OPTIONS, "<image>"],
                "attach": ["hdiutil", "attach", "<image>", "-mountpoint", "<mount>",
                           *ATTACH_OPTIONS],
            },
            "premises": len(PREMISES),
            "preflight": {
                "checks": PREFLIGHT_CHECKS,
                "mode": "product-custody-census",
                "observer": "the barge binary's own custody scan",
                "bounds": PRODUCT_CUSTODY_BOUNDS,
                "probe": {
                    "project": PREFLIGHT_PROJECT,
                    "consumer": PREFLIGHT_CONSUMER,
                },
                "infrastructure_exit": EXIT_INFRASTRUCTURE,
            },
            "premise_2": {
                "real_state": REAL_STATE_RELATIVE,
                "entries": REAL_STATE_ENTRIES,
                "bytes": REAL_STATE_BYTES,
                "deadline_s": REAL_STATE_SECONDS,
                "resolver_proof": "scratch.delegate/scratch.revoke against the fixture registry",
            },
            "supervisor_sha256": SUPERVISOR_SHA256,
            "problems": problems,
            "ok": not problems,
        }, indent=1))
        return 0 if not problems else 1
    if problems:
        print(json.dumps({"schema": SCHEMA, "ok": False, "problems": problems}))
        return 2

    binary = os.environ.get("BARGE_PROOF_BIN")
    if not binary or not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        print(json.dumps({
            "schema": SCHEMA,
            "ok": False,
            "refused": "BARGE_PROOF_BIN must name an executable barge binary",
        }))
        return 2
    receipt_path = options.receipt
    if os.path.isdir(receipt_path):
        receipt_path = os.path.join(receipt_path, "scratch-expiry-receipt.json")
    proof = Proof(binary, receipt_path, options.diagnostic)
    cancelled = None
    premise_failure = None
    try:
        # The image and its fixture are first, because the census has to be the
        # product's own: one probe root is allocated and really deleted through
        # the binary's custody scan before any case. A census the product cannot
        # complete is `infrastructure`, and no case runs, so a capability
        # limitation is never a case result.
        proof.setup()
        proof.preflight = proof.product_census()
        proof.assert_resolver()
        proof.run_cases()
    except Infrastructure as error:
        proof.preflight = error.record
        proof.infrastructure = {"check": error.check, "detail": error.detail}
        proof.fault = error.check
    except Fault as error:
        proof.results.append({"case": "setup", "status": "failed", "kind": error.kind,
                              "detail": error.detail})
        proof.fault = "setup"
    except Exception as error:
        proof.results.append({"case": "setup", "status": "failed", "kind": "exception",
                              "detail": f"{type(error).__name__}: {error}",
                              "trace": traceback.format_exc()[-800:]})
        proof.fault = "setup"
    finally:
        # A cleanup failure is never suppressed: it keeps the image's backing and
        # makes the receipt unqualified, so a fixture that could not be cleaned up
        # can never look like a cleanup that succeeded.
        try:
            cancelled = proof.cleanup()
        except Exception as error:
            cancelled = f"the proof cleanup raised: {type(error).__name__}: {error}"
    # Premise 2's after-observation, and its assertion: the real account state is
    # exactly what it was, both observations are complete, and the resolver's
    # fixture identity was already proved from fixture-only evidence.
    if proof.real_state is not None:
        before = proof.real_state["before"]
        after = real_state_observation()
        proof.real_state["after"] = after
        # Two incomplete observations are never equality: a cap, a deadline, a
        # short read or a raced entry leaves the premise unproven.
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
    if proof.resolver is None:
        premise_failure = (premise_failure or "") + (
            " the state resolver's fixture identity was never asserted"
        )
    receipt = proof.receipt(cancelled)
    receipt["premise_failure"] = premise_failure.strip() if premise_failure else None
    receipt["receipt_path"] = proof.receipt_path
    proof.write_receipt(receipt)
    print(json.dumps({
        "schema": receipt["schema"],
        "qualified": receipt["qualified"],
        "classification": receipt["classification"],
        "infrastructure": receipt["infrastructure"],
        "preflight": receipt["preflight"],
        "real_state": receipt["real_state"],
        "resolver": receipt["resolver"],
        "premise_failure": receipt["premise_failure"],
        "cancelled": receipt["cancelled"],
        "diagnostic": receipt["diagnostic"],
        "receipt_path": receipt["receipt_path"],
        "cases": receipt["cases"],
        "case_evidence": receipt["case_evidence"],
    }, indent=1))
    if receipt["qualified"]:
        return 0
    if receipt["classification"] == "infrastructure":
        return EXIT_INFRASTRUCTURE
    return 3 if options.diagnostic else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
