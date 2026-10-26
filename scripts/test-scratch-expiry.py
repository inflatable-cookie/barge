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

Before any case, `preflight` proves this environment can observe same-uid
processes at all: whether `proc_pidinfo` may read a process of this account is a
property of the host, not of the product, so a host that cannot is classified
`infrastructure` and aborted unqualified (exit 4) instead of being reported as a
case failure. It launches one same-uid sentinel child that stays in the shared
supervisor's session, lists its descriptors and the descriptors of a pre-existing
same-uid process, and reaps the child within a fixed bound.

Every environment hook it reads is listed in `ENVIRONMENT` and recorded in the
receipt, because premise 6 requires naming each one.
"""

import argparse
import ctypes
import errno
import hashlib
import inspect
import json
import os
import plistlib
import select
import shutil
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
# The preflight's one same-uid child. It stays a member of the shared
# supervisor's session, holds only its standard descriptors, starts nothing and
# creates nothing; the parent reaps it well inside its own fixed lifetime.
SENTINEL_READY = "ready"
SENTINEL_LIFETIME_S = 20
SENTINEL_READY_S = 5
SENTINEL_REAP_S = 5
# The two positive preflight checks, in the order they run. A host that cannot
# list a same-uid child, or a pre-existing same-uid process, is `infrastructure`.
PREFLIGHT_CHECKS = ["sentinel_descriptors", "preexisting_descriptors"]
# The same-uid descriptor observation S5's custody scan uses: `libproc`'s
# `proc_pidinfo(PROC_PIDLISTFDS)` over one process. The preflight proves the
# host exposes it; it is never a second product primitive.
PROC_PIDLISTFDS = 1
SCAN_DESCRIPTORS_PER_PROCESS = 4096
# The unprivileged fact custody classifies with, and the preflight's own proof
# that its second probe really is a same-uid process: the kernel process table
# (`sysctl KERN_PROC_ALL`). The struct is left opaque; the read verifies itself
# against this process's own pid and effective uid before any entry is trusted.
CTL_KERN = 1
KERN_PROC = 14
KERN_PROC_ALL = 0
KINFO_PROC_SIZE = 648
KINFO_PID_OFFSET = 40
KINFO_UID_OFFSET = 420
# Exit status for an infrastructure classification: the environment cannot
# observe same-uid processes, so no case ran and nothing here is evidence.
EXIT_INFRASTRUCTURE = 4
SENTINEL_BODY = (
    "import sys, time\n"
    f"sys.stdout.write('{SENTINEL_READY}\\n')\n"
    "sys.stdout.flush()\n"
    f"time.sleep({SENTINEL_LIFETIME_S})\n"
)
CLAIM_IDLE_SECONDS = 14 * 24 * 60 * 60
EFFIGY_KEEP_SECONDS = 24 * 60 * 60

ENVIRONMENT = {
    "TMPDIR": "the guard's owned scratch; every fixture directory is created here",
    "TMP": "the guard's owned scratch, kept equal to TMPDIR",
    "TEMP": "the guard's owned scratch, kept equal to TMPDIR",
    "BARGE_STATE_DIR": "the private fixture state directory this proof creates",
    "BARGE_PROOF_BIN": "the barge binary this proof exercises",
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
    "the account home used for state resolution is the fixture's; the real state directory's digest is asserted unchanged",
    "BARGE_STATE_DIR, TMPDIR, TMP and TEMP all point inside the guard's owned scratch",
    "every fixture volume is registered by this proof's own explicit registry write, never by a default",
    "one owned sparse image with -owners on, attached at a unique /Volumes name; a forced detach only of the owned image's own whole-disk device",
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


class _FdInfo(ctypes.Structure):
    """`struct proc_fdinfo` from `<libproc.h>`: one descriptor and its type."""

    _fields_ = [("proc_fd", ctypes.c_int32), ("proc_fdtype", ctypes.c_uint32)]


_LIBPROC = None
_LIBKERNEL = None


def libproc():
    """The one `libproc` handle, loaded and typed once."""
    global _LIBPROC
    if _LIBPROC is None:
        library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        library.proc_pidinfo.argtypes = [
            ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int,
        ]
        library.proc_pidinfo.restype = ctypes.c_int
        _LIBPROC = library
    return _LIBPROC


def list_descriptors(pid, cap=SCAN_DESCRIPTORS_PER_PROCESS):
    """One process's descriptor numbers, by S5's own custody primitive.

    This is `proc_pidinfo(PROC_PIDLISTFDS)` over one pid, the call
    `crates/barge/src/os/macos.rs` reads. Zero bytes is "no descriptors" only
    when the call left no error: every other failure is raised, because reading
    an unreadable process as "nothing open" is the very mistake the preflight
    exists to catch. A bounding cap answers a full table rather than an
    unbounded read.
    """
    buffer = (_FdInfo * cap)()
    ctypes.set_errno(0)
    written = libproc().proc_pidinfo(
        pid, PROC_PIDLISTFDS, 0, ctypes.byref(buffer), ctypes.sizeof(buffer)
    )
    error = ctypes.get_errno()
    if written <= 0:
        if error == 0:
            return []
        raise OSError(error, os.strerror(error))
    count = written // ctypes.sizeof(_FdInfo)
    return [buffer[index].proc_fd for index in range(count)]


def libkernel():
    """The one libSystem handle for the kernel table read, typed once."""
    global _LIBKERNEL
    if _LIBKERNEL is None:
        library = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
        library.sysctl.argtypes = [
            ctypes.POINTER(ctypes.c_int), ctypes.c_uint, ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t,
        ]
        library.sysctl.restype = ctypes.c_int
        _LIBKERNEL = library
    return _LIBKERNEL


def process_table():
    """The kernel's own process table, one `sysctl(KERN_PROC_ALL)` read.

    This is the unprivileged fact custody classifies with: every process's pid
    and effective uid, obtained without inspecting any process. The read asks
    for the size, then reads with slack for processes that appear in between,
    and a table that is not a whole number of entries is refused rather than
    parsed into a wrong uid.
    """
    mib = (ctypes.c_int * 3)(CTL_KERN, KERN_PROC, KERN_PROC_ALL)
    size = ctypes.c_size_t(0)
    ctypes.set_errno(0)
    if libkernel().sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    buffer = ctypes.create_string_buffer(size.value + 16384)
    read = ctypes.c_size_t(size.value + 16384)
    ctypes.set_errno(0)
    if libkernel().sysctl(mib, 3, buffer, ctypes.byref(read), None, 0) != 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    raw = buffer.raw[:read.value]
    if len(raw) % KINFO_PROC_SIZE:
        raise OSError(f"the kernel process table is not a whole number of entries: {len(raw)} bytes")
    return raw


def table_entries(raw):
    """`{pid: effective uid}` from the kernel's own table."""
    entries = {}
    for index in range(len(raw) // KINFO_PROC_SIZE):
        entry = raw[index * KINFO_PROC_SIZE:(index + 1) * KINFO_PROC_SIZE]
        pid = int.from_bytes(
            entry[KINFO_PID_OFFSET:KINFO_PID_OFFSET + 4], sys.byteorder
        )
        uid = int.from_bytes(
            entry[KINFO_UID_OFFSET:KINFO_UID_OFFSET + 4], sys.byteorder
        )
        entries[pid] = uid
    return entries


def choose_preexisting(parent, own_pid, own_uid, exclude, table, uid_of):
    """Pick the pre-existing same-uid process the second descriptor probe reads.

    The parent — the shared supervisor that runs this proof — is preferred; a
    fallback comes from the kernel table in table order. A candidate's effective
    uid is a kinfo fact (`uid_of`), never an assumption about who forked this
    process, and neither this process nor the preflight's own sentinel is ever
    chosen. Returns `(pid, source)` or `(None, reason)`.
    """
    skipped = {own_pid, *exclude}
    if parent not in skipped and uid_of(parent) == own_uid:
        return parent, "parent"
    for pid in table:
        if pid in skipped:
            continue
        if uid_of(pid) == own_uid:
            return pid, "kernel table"
    return None, "the kernel table names no other process of this effective uid"


def verified_entries(raw, own_pid, own_uid):
    """`{pid: effective uid}` from the kernel table, after it proves itself.

    The table must describe *this* process as this pid and this effective uid
    before any entry is trusted: an entry size or offset that disagrees with
    `getpid` and `geteuid` would otherwise classify another account's process as
    this one's. A table that disagrees is refused, never parsed further.
    """
    entries = table_entries(raw)
    if entries.get(own_pid) != own_uid:
        raise OSError(
            f"the kernel process table does not describe this process ({own_pid}) as "
            f"uid {own_uid}, so its offsets cannot be trusted"
        )
    return entries


def preexisting_process(exclude=()):
    """A pre-existing same-uid process, its uid read from the kernel table."""
    own_pid, own_uid = os.getpid(), os.geteuid()
    entries = verified_entries(process_table(), own_pid, own_uid)
    pid, source = choose_preexisting(
        os.getppid(), own_pid, own_uid, tuple(exclude), list(entries), entries.get
    )
    if pid is None:
        raise OSError(source)
    return pid, source


class Sentinel:
    """The preflight's own same-uid child, and nothing else.

    It is started without a session of its own (`no start_new_session`, no
    `preexec_fn`), so the shared supervisor's census keeps it as a member and
    can terminate it; its body sleeps for the declared bound and starts nothing.
    """

    def __init__(self, process):
        self.process = process
        self.pid = process.pid
        self.session = None

    def await_ready(self, budget=SENTINEL_READY_S):
        """Wait, bounded, for the child's readiness line on its stdout pipe."""
        ready, _, _ = select.select([self.process.stdout], [], [], budget)
        if not ready:
            raise OSError(f"the sentinel child did not report ready within {budget:g}s")
        line = self.process.stdout.readline()
        if line.strip() != SENTINEL_READY.encode():
            raise OSError(f"the sentinel child reported {line[:80]!r}, not readiness")

    def close(self):
        if self.process.stdout is not None and not self.process.stdout.closed:
            self.process.stdout.close()


def spawn_sentinel(env=None):
    """Start the sentinel child in the shared supervisor's own session."""
    process = subprocess.Popen(
        [sys.executable, "-c", SENTINEL_BODY],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        close_fds=True,
    )
    sentinel = Sentinel(process)
    try:
        sentinel.session = os.getsid(process.pid)
        sentinel.await_ready()
    except BaseException:
        # A child that never became ready is still this proof's to clean up:
        # bound the reap here, then let the preflight refuse.
        reap_sentinel(sentinel)
        raise
    return sentinel


def reap_sentinel(sentinel, budget=SENTINEL_REAP_S):
    """Terminate and reap this proof's own child within a fixed bound.

    Only the sentinel's own pid is signalled, never a group and never a pattern,
    so nothing the shared supervisor or another task owns is touched. The result
    is recorded either way: an un-reaped child is an infrastructure refusal, not
    a silent leak.
    """
    started = time.time()
    result = {"reaped": False, "signal": None, "ms": 0}
    process = sentinel.process
    try:
        if process.poll() is None:
            process.terminate()
            result["signal"] = "SIGTERM"
        try:
            process.wait(timeout=budget)
        except subprocess.TimeoutExpired:
            process.kill()
            result["signal"] = "SIGKILL"
            process.wait(timeout=budget)
        result["reaped"] = process.returncode is not None
    except subprocess.TimeoutExpired:
        result["reaped"] = False
        result["detail"] = f"the sentinel did not exit within {budget:g}s"
    except OSError as error:
        result["reaped"] = False
        result["detail"] = f"the sentinel could not be reaped: {error}"
    finally:
        sentinel.close()
        result["ms"] = int((time.time() - started) * 1000)
    return result


def preflight(spawn=spawn_sentinel, describe=list_descriptors, reap=reap_sentinel,
              session_of=os.getsid, preexisting=preexisting_process):
    """Prove this host can observe same-uid processes, before any case.

    `proc_pidinfo` answers `EPERM` for some same-effective-uid processes on some
    hosts, and custody correctly reads an unreadable process as Unknown and
    refuses. Whether this account may read its own processes is a property of the
    environment, not of the product, so the proof establishes it first: a host
    that cannot is `infrastructure`, unqualified, aborted before any case, and no
    case body ever runs on unproven ground.

    Two descriptor observations, both with the primitive S5's scan uses, and both
    required — an identity, session or pin check is supplemental and can never
    replace them:
    1. the descriptors of a same-uid child this proof launches (`sentinel`), which
       must have stayed in the shared supervisor's session and be reaped within a
       fixed bound;
    2. the descriptors of a pre-existing same-uid process this proof did not
       start, whose effective uid is a kernel-table fact, never an assumption.
    """
    record = {"checks": [], "sentinel": None, "reap": None}
    failure = None
    sentinel = None
    try:
        record["session"] = session_of(0)
    except OSError as error:
        record["session"] = None
        failure = ("session", f"the proof cannot read its own session: {error}")
    if failure is None:
        try:
            sentinel = spawn()
        except (OSError, subprocess.SubprocessError) as error:
            failure = ("sentinel_launch", f"the same-uid sentinel child did not start: {error}")
    if failure is None:
        record["sentinel"] = {"pid": sentinel.pid, "session": sentinel.session}
        if sentinel.session != record["session"]:
            failure = (
                "sentinel_session",
                f"the sentinel runs in session {sentinel.session}, not the guard's "
                f"{record['session']}",
            )
        else:
            try:
                descriptors = describe(sentinel.pid)
            except OSError as error:
                failure = (
                    "sentinel_descriptors",
                    f"the sentinel's descriptors are unreadable: {error}",
                )
            else:
                record["checks"].append({
                    "check": "sentinel_descriptors", "status": "passed",
                    "pid": sentinel.pid, "descriptors": descriptors,
                })
                if 1 not in descriptors:
                    failure = (
                        "sentinel_descriptors",
                        "the sentinel's listing omits its own stdout pipe, so the listing "
                        "is not that child's table",
                    )
    if failure is None:
        try:
            pid, source = preexisting(tuple([sentinel.pid]) if sentinel else ())
            descriptors = describe(pid)
        except OSError as error:
            failure = (
                "preexisting_descriptors",
                f"a pre-existing same-uid process could not be observed: {error}",
            )
        else:
            record["checks"].append({
                "check": "preexisting_descriptors", "status": "passed",
                "pid": pid, "uid": os.geteuid(), "uid_source": source,
                "descriptors": descriptors,
            })
            if not descriptors:
                failure = (
                    "preexisting_descriptors",
                    "the pre-existing same-uid process listed no descriptors",
                )
    if sentinel is not None:
        record["reap"] = reap(sentinel)
        if failure is None and not record["reap"].get("reaped"):
            failure = (
                "sentinel_reap",
                f"the sentinel child was not reaped within its bound: {record['reap']}",
            )
    if failure is not None:
        record["failed"] = {"check": failure[0], "detail": failure[1]}
        raise Infrastructure(failure[0], failure[1], record)
    return record


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
        self.attached = False
        self.env = None

    # -- fixture setup ----------------------------------------------------

    def setup(self):
        scratch = os.environ.get("TMPDIR")
        if not scratch or not os.path.isdir(scratch):
            raise Fault("premise", "TMPDIR is not a directory the guard created")
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
        first = self.alloc(project, "first")
        second = self.alloc(project, "second")
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
        leaf = os.path.join(folder, pending[0]["quarantine_name"])
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
        leaf = None
        try:
            leaf = self.wait_for_quarantine(folder, timeout=60)
            # Premise 5: a forced detach only of the owned image's own whole disk.
            run(["hdiutil", "detach", "-force", f"/dev/{self.whole_disk}"], self.env, timeout=120)
            self.attached = False
        finally:
            try:
                process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                process.kill()
                raise Fault("case", "the expiry did not stop after the volume was pulled")
        pending = self.pending_intents()
        if len(pending) != 1:
            raise Fault("case", f"expected one pending intent after the pull, found {len(pending)}")
        kept = os.path.join(folder, pending[0]["quarantine_name"])
        if not os.path.isdir(kept):
            raise Fault("case", "the quarantine leaf was not kept through the pull")
        if not os.path.isfile(os.path.join(kept, ".barge-root.json")):
            raise Fault("case", "the kept leaf lost its marker")
        self.reattach()
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
        return "an ordinary pull mid-delete kept the leaf; the replay after re-attach settled it"

    def case_a_stale_mount_point_is_never_written(self):
        self.detach()
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
            folder = os.path.join(self.mount, intent["folder"])
            leaf = os.path.join(folder, intent["quarantine_name"])
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
        if not self.attached:
            return
        result = run(["hdiutil", "detach", self.mount], self.env, timeout=180, check=False)
        if result.returncode != 0:
            result = run(["hdiutil", "detach", "-force", self.mount], self.env, timeout=180)
        self.attached = False

    # -- the run ----------------------------------------------------------

    def run_cases(self):
        passed = set()
        for name, needs in CASES:
            missing = [need for need in needs if need not in passed]
            skip = Skip(f"a dependency did not pass: {', '.join(missing)}") if missing else None
            if skip is None and self.fault is not None:
                skip = Skip("a diagnostic fault is latched; only the fault's dependents are skipped")
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
        return {
            "schema": SCHEMA,
            "qualified": (classification == "proof"
                          and not self.diagnostic
                          and self.fault is None
                          and cancelled is None
                          and all(row["status"] == "passed" for row in self.results)
                          and len(self.results) == len(CASES)),
            "classification": classification,
            "infrastructure": self.infrastructure,
            "preflight": self.preflight,
            "diagnostic": self.diagnostic,
            "cancelled": cancelled,
            "wall_limit_ms": WALL_MS,
            "scratch_budget_mib": SCRATCH_MIB,
            "image_mib": IMAGE_MIB,
            "cases": self.results,
            "case_evidence": self.case_evidence,
            "premises": PREMISES,
            "environment": sorted(ENVIRONMENT),
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
        try:
            self.detach()
        finally:
            if self.root and os.path.isdir(self.root):
                shutil.rmtree(self.root, ignore_errors=True)


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
    """Pure tests of the preflight: both positives and an abort on either failure.

    No child, no image and no descriptor of a real process: the spawner, the
    listing and the reap are injected. A failing check must raise
    `Infrastructure` naming the check, and a sentinel that was started must be
    reaped on every path, including the refusals, so cleanup stays bounded when
    the preflight refuses.
    """
    problems = []
    sentinel_pid, preexisting_pid, session = 4242, 4343, 99

    class FakeSentinel:
        """A sentinel that owns no process at all."""

        def __init__(self, pid=sentinel_pid, session_id=session):
            self.pid = pid
            self.session = session_id
            self.process = None

    def run(describe, spawn=None, reap=None,
            preexisting=lambda exclude: (preexisting_pid, "parent")):
        spawned = []
        reaped = []

        def fake_spawn():
            sentinel = spawn() if spawn is not None else FakeSentinel()
            spawned.append(sentinel)
            return sentinel

        def fake_reap(sentinel):
            outcome = {"reaped": True, "signal": "SIGTERM", "ms": 1}
            if reap is not None:
                outcome = reap(sentinel)
            reaped.append((sentinel, outcome))
            return outcome

        try:
            record = preflight(
                spawn=fake_spawn,
                describe=describe,
                reap=fake_reap,
                session_of=lambda pid: session,
                preexisting=preexisting,
            )
        except Infrastructure as error:
            return None, error, spawned, reaped
        return record, None, spawned, reaped

    def listable(pid):
        return [0, 1, 2]

    # The pre-existing candidate is chosen from verified same-uid facts only:
    # the parent preferred, the kernel table as fallback, never this process,
    # never the preflight's own sentinel.
    own_pid, own_uid = 10, 501
    facts = {99: 501, 11: 501, 12: 0, own_pid: own_uid}
    if choose_preexisting(99, own_pid, own_uid, (), [11, 12], facts.get) != (99, "parent"):
        problems.append("the pre-existing probe does not prefer a same-uid parent")
    facts[99] = 0
    if choose_preexisting(99, own_pid, own_uid, (), [11, 12], facts.get) != (11, "kernel table"):
        problems.append("the pre-existing probe does not fall back to the kernel table")
    facts[99] = own_uid
    if choose_preexisting(99, own_pid, own_uid, (99,), [11, 12], facts.get) != (11, "kernel table"):
        problems.append("the pre-existing probe can choose the preflight's own sentinel")
    if choose_preexisting(98, own_pid, own_uid, (), [own_pid, 12], facts.get)[0] is not None:
        problems.append("the pre-existing probe chose a pid with no same-uid process")
    if choose_preexisting(98, own_pid, own_uid, (), [own_pid, 12], facts.get)[1] == "":
        problems.append("a table without a same-uid process gives no reason")

    # The kinfo read proves itself before any entry is trusted, and is the
    # product's own unprivileged classification fact.
    if KINFO_PROC_SIZE != 648:
        problems.append("the kinfo entry size changed")
    for offset in (KINFO_PID_OFFSET, KINFO_UID_OFFSET):
        if not 0 < offset <= KINFO_PROC_SIZE - 4:
            problems.append(f"the kinfo offset {offset} is outside the entry")
    if "KERN_PROC_ALL" not in inspect.getsource(process_table):
        problems.append("the preflight no longer reads the kernel process table")

    # The kinfo read is exercised purely on a hand-built table at the declared
    # offsets: agreeing entries parse, and a table that does not describe this
    # process as itself is refused before any uid is trusted.
    def raw_table(rows):
        raw = bytearray()
        for pid, uid in rows:
            entry = bytearray(KINFO_PROC_SIZE)
            entry[KINFO_PID_OFFSET:KINFO_PID_OFFSET + 4] = pid.to_bytes(4, sys.byteorder)
            entry[KINFO_UID_OFFSET:KINFO_UID_OFFSET + 4] = uid.to_bytes(4, sys.byteorder)
            raw.extend(entry)
        return bytes(raw)

    if verified_entries(raw_table([(10, 501), (11, 0)]), 10, 501) != {10: 501, 11: 0}:
        problems.append("the kinfo table does not parse the pid and uid it was built with")
    bad_tables = [
        [(10, 502), (11, 0)],  # this pid, another uid
        [(11, 0)],             # this pid absent
    ]
    for rows in bad_tables:
        try:
            verified_entries(raw_table(rows), 10, 501)
        except OSError:
            pass
        else:
            problems.append("a kinfo table that does not describe this process was trusted")
    try:
        verified_entries(raw_table([(10, 501)])[:-1], 10, 501)
    except OSError:
        pass
    else:
        problems.append("a kinfo table that is not whole entries was parsed")

    # Both checks pass: the same-uid child and a pre-existing same-uid process.
    record, error, spawned, reaped = run(listable)
    if record is None:
        problems.append(
            f"a host that can list same-uid descriptors was refused: {error.detail}"
        )
    else:
        checks = [check["check"] for check in record["checks"]]
        if checks != PREFLIGHT_CHECKS:
            problems.append(f"the positive preflight records {checks}, not {PREFLIGHT_CHECKS}")
        for check in record["checks"]:
            if check["status"] != "passed" or 1 not in check["descriptors"]:
                problems.append(
                    f"the positive preflight's `{check['check']}` is not a passed listing"
                )
        if not record["reap"] or not record["reap"].get("reaped"):
            problems.append("the positive preflight did not reap its sentinel")
        second = record["checks"][-1]
        if second.get("uid") != os.geteuid() or not second.get("uid_source"):
            problems.append("the pre-existing probe records no verified same-uid fact")
    if len(spawned) != 1 or len(reaped) != 1:
        problems.append("the positive preflight did not start and reap exactly one sentinel")

    def unreadable(broken):
        def describe(pid):
            if broken == "sentinel" and pid == sentinel_pid:
                raise OSError(errno.EPERM, "Operation not permitted")
            if broken == "preexisting" and pid == preexisting_pid:
                raise OSError(errno.EPERM, "Operation not permitted")
            return [0, 1, 2]
        return describe

    # Either listing failing is infrastructure: unqualified, aborted before any
    # case, and still cleaned up.
    for broken, expected in (("sentinel", "sentinel_descriptors"),
                             ("preexisting", "preexisting_descriptors")):
        record, error, spawned, reaped = run(unreadable(broken))
        if record is not None:
            problems.append(f"an unreadable {broken} same-uid process was not refused")
            continue
        if error.check != expected:
            problems.append(f"the {broken} failure is `{error.check}`, not `{expected}`")
        if not error.detail:
            problems.append(f"the {broken} refusal names no reason")
        if not error.record.get("failed", {}).get("check"):
            problems.append(f"the {broken} refusal kept no failed-check record")
        if len(spawned) != 1 or len(reaped) != 1 or not reaped[0][1].get("reaped"):
            problems.append(f"the {broken} refusal did not bound and reap its sentinel")

    # A listing that answers but is not the sentinel's own table (no stdout
    # pipe) is a refusal, not a pass: `proc_pidinfo` answers zero bytes with no
    # error for a process with no descriptors, and that must not be evidence.
    def sentinel_without_table(pid):
        return [0, 2] if pid == sentinel_pid else [0, 1, 2]

    record, error, spawned, reaped = run(sentinel_without_table)
    if record is not None or error.check != "sentinel_descriptors":
        problems.append("a sentinel listing without its own stdout pipe was not refused")

    def empty_preexisting(pid):
        return [0, 1, 2] if pid == sentinel_pid else []

    record, error, spawned, reaped = run(empty_preexisting)
    if record is not None or error.check != "preexisting_descriptors":
        problems.append("an empty pre-existing listing was not refused")

    # A host with no other process of this effective uid cannot run the second
    # required observation, so it is infrastructure, not a pass.
    def no_other_same_uid(exclude):
        raise OSError("the kernel table names no other process of this effective uid")

    record, error, spawned, reaped = run(listable, preexisting=no_other_same_uid)
    if record is not None or error.check != "preexisting_descriptors":
        problems.append("a host with no pre-existing same-uid process was not refused")

    # A sentinel that left the shared supervisor's session is refused and reaped.
    record, error, spawned, reaped = run(
        listable, spawn=lambda: FakeSentinel(session_id=session + 1)
    )
    if record is not None or error.check != "sentinel_session":
        problems.append("a sentinel outside the shared supervisor's session was not refused")
    elif len(reaped) != 1 or not reaped[0][1].get("reaped"):
        problems.append("the escaped sentinel was not reaped")

    # A child that never becomes ready is a launch failure, so the preflight
    # refuses instead of listing something else.
    def never_ready():
        raise OSError("the sentinel child did not report ready within 5s")

    record, error, spawned, reaped = run(listable, spawn=never_ready)
    if record is not None or error.check != "sentinel_launch":
        problems.append("a sentinel that never became ready was not a launch refusal")

    # A sentinel that outlives its reap bound is infrastructure too: a refused
    # preflight must not leave a child behind.
    def unreapable(sentinel):
        return {"reaped": False, "signal": "SIGKILL", "ms": SENTINEL_REAP_S * 1000}

    record, error, spawned, reaped = run(listable, reap=unreapable)
    if record is not None or error.check != "sentinel_reap":
        problems.append("an unreaped sentinel was not refused as infrastructure")

    # An infrastructure receipt: unqualified, no case rows, mode preserved.
    proof = Proof("/nonexistent/barge", None, False)
    proof.infrastructure = {"check": "sentinel_descriptors", "detail": "Operation not permitted"}
    proof.preflight = {"checks": [], "failed": dict(proof.infrastructure)}
    receipt = proof.receipt()
    if receipt["classification"] != "infrastructure" or receipt["qualified"]:
        problems.append("an infrastructure receipt is not unqualified infrastructure")
    if receipt["cases"]:
        problems.append("an infrastructure abort recorded case rows")
    if receipt["infrastructure"] is None or receipt["preflight"] is None:
        problems.append("an infrastructure receipt names neither its check nor its preflight")

    # The sentinel's containment and bounds, taken from the source it runs.
    if f"time.sleep({SENTINEL_LIFETIME_S})" not in SENTINEL_BODY:
        problems.append("the sentinel body does not sleep for the declared lifetime")
    for forbidden in ("setsid", "fork", "subprocess", "os.", "Popen"):
        if forbidden in SENTINEL_BODY:
            problems.append(f"the sentinel body names `{forbidden}`, so it could leave the session")
    spawn_source = inspect.getsource(spawn_sentinel)
    for forbidden in ("start_new_session", "preexec_fn", "setsid"):
        if forbidden in spawn_source:
            problems.append(f"the sentinel is spawned with `{forbidden}`")
    if not SENTINEL_LIFETIME_S > SENTINEL_READY_S + SENTINEL_REAP_S:
        problems.append("the sentinel can outlive its own ready and reap bounds")
    if SENTINEL_REAP_S > 30:
        problems.append("the sentinel's reap bound is not short")
    if "PROC_PIDLISTFDS" not in inspect.getsource(list_descriptors):
        problems.append("the preflight no longer lists descriptors with PROC_PIDLISTFDS")
    if PREFLIGHT_CHECKS != ["sentinel_descriptors", "preexisting_descriptors"]:
        problems.append("the preflight check names changed")
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
    for name, needs in CASES:
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
    for hook in ("TMPDIR", "BARGE_STATE_DIR", "BARGE_PROOF_BIN", "HOME", "CARGO_HOME"):
        if hook not in ENVIRONMENT:
            problems.append(f"the environment record omits {hook}")
    for hook in ENVIRONMENT.values():
        if not hook:
            problems.append("an environment hook has no recorded purpose")
    if not set(ENVIRONMENT) <= {
        "TMPDIR", "TMP", "TEMP", "BARGE_STATE_DIR", "BARGE_PROOF_BIN", "HOME", "CARGO_HOME",
    }:
        problems.append("the environment record names a hook the case script does not read")
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
                "descriptor_primitive": "proc_pidinfo(PROC_PIDLISTFDS)",
                "uid_primitive": "sysctl(KERN_PROC_ALL) kinfo_proc",
                "sentinel": {
                    "ready_s": SENTINEL_READY_S,
                    "lifetime_s": SENTINEL_LIFETIME_S,
                    "reap_s": SENTINEL_REAP_S,
                },
                "infrastructure_exit": EXIT_INFRASTRUCTURE,
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
    try:
        # Before any case, and before the image: prove this host lets the account
        # read a same-uid process. An unobservable host is `infrastructure`, and
        # no case runs, so a capability limitation is never a case result.
        proof.preflight = preflight()
        proof.setup()
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
        try:
            proof.cleanup()
        except Exception:
            cancelled = None
    receipt = proof.receipt(cancelled)
    receipt["receipt_path"] = proof.receipt_path
    proof.write_receipt(receipt)
    print(json.dumps({
        "schema": receipt["schema"],
        "qualified": receipt["qualified"],
        "classification": receipt["classification"],
        "infrastructure": receipt["infrastructure"],
        "preflight": receipt["preflight"],
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
