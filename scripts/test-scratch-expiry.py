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
  --self-check   check this script's own case map, premise record and pinned
                 supervisor digest; no image, no scratch, no child.

Every environment hook it reads is listed in `ENVIRONMENT` and recorded in the
receipt, because premise 6 requires naming each one.
"""

import argparse
import hashlib
import json
import os
import plistlib
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
WALL_MS = 300_000
SCRATCH_MIB = 512
NATIVE_AGE_DAYS = 7
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
        self.fault = None
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
        run(
            [
                "hdiutil", "create", "-size", f"{IMAGE_MIB}m", "-fs", "APFS",
                "-type", "SPARSE", "-volname", "barge-proof-scratch",
                "-owners", "on", self.image,
            ],
            self.env,
        )
        name = f"barge-proof-{os.getpid()}-{int(time.time()) % 100000}"
        self.mount = f"/Volumes/{name}"
        run(
            [
                "hdiutil", "attach", self.image, "-mountpoint", self.mount,
                "-nobrowse", "-owners", "on",
            ],
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
            raise Fault("case", f"the expiry deleted {real['data']['deleted']['count']} roots")
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
        run(["hdiutil", "attach", self.image, "-mountpoint", self.mount,
             "-nobrowse", "-owners", "on"], self.env, timeout=180)
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
        return {
            "schema": SCHEMA,
            "qualified": (not self.diagnostic
                          and self.fault is None
                          and cancelled is None
                          and all(row["status"] == "passed" for row in self.results)
                          and len(self.results) == len(CASES)),
            "diagnostic": self.diagnostic,
            "cancelled": cancelled,
            "wall_limit_ms": WALL_MS,
            "scratch_budget_mib": SCRATCH_MIB,
            "image_mib": IMAGE_MIB,
            "cases": self.results,
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


def self_check():
    """The proof's own apparatus: the case map and its dispatch, pins, premises.

    The dispatch check is the guard that was missing when eight of nine case
    names had no method: `run_cases` calls `case_<name>`, so the self-check
    proves every case name resolves to a method and every case method is in the
    map, and that each dependency is evaluated before the case that needs it.
    """
    problems = []
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
            "premises": len(PREMISES),
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
        proof.setup()
        proof.run_cases()
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
        "diagnostic": receipt["diagnostic"],
        "receipt_path": receipt["receipt_path"],
        "cases": receipt["cases"],
    }, indent=1))
    if receipt["qualified"]:
        return 0
    return 3 if options.diagnostic else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
