#!/usr/bin/env python3
"""Barge F6-R1's native proof: the release effect on one owned synthetic APFS image.

  python3 scripts/native/release-proof.py stage         # build and stage the proof binary, effect-free
  python3 scripts/native/release-proof.py counted       # the author pass (owner-activated)
  python3 scripts/native/release-proof.py diagnostic    # the uncounted twin (owner-activated)
  python3 scripts/native/release-proof.py --self-check  # apparatus only, effect-free
  python3 scripts/native/release-proof.py --plan        # the plan, effect-free

It is owner-activated. No ordinary Effigy selector runs a counted or diagnostic pass,
and the declarations activate nothing. The binary under test is never named by the
caller: the guard's own child stages a proof-feature build of HEAD inside the run
root the guard created, hashes it, verifies the source-bound manifest, and runs only
that staged file. No artifact from another run, and no shared stage directory, is
accepted.

What it does, in one sentence: after the unchanged qualified supervisor's bytes and
this host's Stash scratch route are verified, and under that supervisor (pinned below,
executed with the same interpreter), it creates one private run root inside the
guard's owned scratch, attaches one owned APFS sparse image of 64 MiB with ownership
enabled, registers that image as the only volume, and runs sixteen release cases
through the staged `barge` CLI against a private HOME, a private state directory and a
synthetic account home (BARGE_ACCOUNT_HOME, equal to HOME). Each case declares its own
area, so one case's roots never reach another case's plan.

The staged binary is the `proof-synthetic-account` feature build. Under that feature
alone, Oxide's reference directories come from the synthetic account home. The
ordinary production build is a different binary and this proof does not run it. The
receipt records that difference as a limitation (LIMITATION below).

What it never does:

  - it never stages or builds before the supervisor's own bytes and the scratch
    route are verified, and staging spends the supervisor's 300 s wall, so no
    unguarded prewarm or clock reset exists;
  - it never touches the real account home or the real Barge state directory, and it
    never registers a real drive (Builds included);
  - it never reads or writes Oxide's real account control: the staged build resolves
    Oxide's directories from the synthetic home only, and every case records the
    reference control it chose, which must lie inside the owned scratch;
  - it never deletes anything but synthetic roots inside its own image;
  - it never force-detaches; detach is the ordinary `hdiutil detach`, authorized by a
    fresh rebind census each time, and closure needs the child's success and reap plus
    whole-backing attachment absence, not merely a missing mount path;
  - it publishes the guard's opt-in `image-custody.json` indication BEFORE the backing
    exists, so the guard's own cleanup keeps the root while the backing is mounted or
    attached-but-unmounted;
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
# The one volume and scratch parent this proof uses. The volume's UUID is verified
# before any staging or fixture effect, and the scratch parent must be the canonical,
# owner-only barge scratch directly under that mount: no drive root, no home folder,
# no symlinked component and no other volume. Historical Builds evidence stays
# unavailable history and is never reconstructed here.
DESIGNATED_SCRATCH = "/Volumes/Stash/scratch/barge"
SCRATCH_VOLUME_MOUNT = "/Volumes/Stash"
SCRATCH_VOLUME_UUID = "8A660440-C919-45C6-A3F1-3365B9BD2006"
# The guest route is separate from the host Stash identity.  These constants do
# not activate a guest occurrence or authenticate the host's instruction record.
GUEST_VM_UUID = "ddf1f166-4796-4f40-925e-eaf35172c63f"
GUEST_VOLUME_UUID = "74097A39-2EDF-415E-BA9D-9753E0343AD9"
GUEST_MOUNT = "/System/Volumes/Data"
GUEST_PARENT = "/System/Volumes/Data/Users/Shared"
GUEST_UID = 501
GUEST_GID = 20
GUEST_HOME = "/Users/inflatable-tester"
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
# Oxide's coordinated single-authority registry: the /4 documents Barge's
# generic release and census readers must read strictly (settled required,
# deletion_routes gone).
OXIDE_COORDINATED_REGISTRY_VERSION = "oxide.outputs.registry/4"
# Oxide's coordinated control fence (`COORDINATED_OUTPUTS_FENCE`) and the
# predecessor `/4` fence. The coordinated class is a paired, completed handoff:
# both pool controls record it `ready` under one transaction before the `/4`
# registry image exists, and that image's `created_by` is the same transaction.
OXIDE_COORDINATED_FENCE = "oxide.outputs/5"
OXIDE_ROUTED_FENCE = "oxide.outputs/4"
# The one adoption transaction this proof's fixtures record, as Oxide's own
# 32-hex nonce spells it.
OXIDE_TRANSACTION = "a" * 32
# The internal pool's control directory under the synthetic home: the default
# pool the proof build resolves, whose own `removable/` child is the second
# control a registered pair writes.
OXIDE_RELATIVE_CONTROL = os.path.dirname(os.path.dirname(OXIDE_RELATIVE_REGISTRY))
# The registration's own name inside that control directory, and the synthetic
# pool volume UUID its field set carries (a UUID this fixture names, never a
# real drive).
OXIDE_REGISTRATION_NAME = "removable.json"
OXIDE_POOL_VOLUME_UUID = "8a660440-c919-45c6-a3f1-3365b9bd2006"
VOLUME_UUID_KEY = "VolumeUUID"
HOLDER = "release-native-holder"
PROOF_RECEIPT = "release-proof-receipt.json"
# How much of a plan row a failing case records after its deciding fields. The fields
# that decide eligibility are recorded first and in full, because the CLI nests a
# sorted-key document whose `absence` member alone is larger than a short cap; the raw
# row follows, bounded, so one huge row cannot bloat the receipt.
ROW_EVIDENCE_CHARS = 2000
# How much of one free-text value a failure record keeps; the marker states the remainder.
FAILURE_TEXT_CHARS = 400
# A plan-row failure record keeps every deciding field, so each free-text field is
# capped on its own here: no single long reason can push another gate's verdict out of
# the record. The raw row that follows uses ROW_EVIDENCE_CHARS.
ROW_FAILURE_FIELD_CHARS = 240
# Any holder or name that is not this proof's own is refused.
NAMESPACE = "example"
# The one Cargo feature the staged binary is built with. It is named here, in the
# build command, the staged manifest and the self-check's manifest reading.
PROOF_FEATURE = "proof-synthetic-account"
# `--offline` and `--locked` refuse a network fetch and a lockfile change, so the
# staged bytes come from the committed lock on the pinned toolchain.
BUILD_COMMAND = [
    "oxide", "build", "-p", "barge-cli", "--locked", "--offline",
    "--features", PROOF_FEATURE, "--message-format", "json",
]
TOOLCHAIN_CHANNEL = "1.97.1"
# The staged copy lives inside this occurrence's guarded root, never a shared stage
# directory and never outside the guard, and its directory name is exclusive.
STAGE_PREFIX = "proof-binary-"
# The explicit, separately selected `release:native:stage` build is the only one that
# may take longer than the guard's wall; it is still bounded.
STAGE_LIMIT_SECONDS = 1800
STAGE_MANIFEST = "proof-binary.json"
STAGE_SCHEMA = "barge.release.proof-binary/1"
# The guard hands the child its own 300 s wall. The outer half publishes the same
# absolute monotonic deadline it launched with, so staging and its waits spend the
# guard's bound and never a fresh clock of their own.
WALL_DEADLINE_ENV = "BARGE_PROOF_WALL_DEADLINE_NS"
# The verified scratch route, as a document the guarded half re-checks.
ROUTE_ENV = "BARGE_PROOF_ROUTE"
ROUTE_SCHEMA = "barge.release.proof-route/1"
# The guard's opt-in image-custody indication (oxide#191): one bounded atomic
# document naming every attachable backing this run can create. It is published
# BEFORE the backing exists and stays for the run, so the guard's own cleanup sees
# the backing and keeps the root while it is mounted or attached-but-unmounted.
CUSTODY_NAME = "image-custody.json"
CUSTODY_VERSION = 1
# The attachment census report and the bounded raw prefix each command record keeps.
ATTACHMENT_CENSUS_SCHEMA = "barge.release.attachment-census/1"
COMMAND_SCHEMA = "barge.release.command/1"
CENSUS_PREFIX_BYTES = 4096
# One barge-command fault record keeps the command's own captured evidence first and the
# returned document's deciding fields next, each inside its own bound; the raw stream
# prefixes come last, so no leading member can hide a counter, root id or reason. Those
# prefixes are published whether or not any document parsed: the captured bytes are the
# only explanation of a parse, decode or exit fault.
BARGE_FAULT_DETAIL_CHARS = 16384
# Every command-evidence field the census copies from its own command record, both streams
# and the incompleteness facts included.
COMMAND_EVIDENCE_FIELDS = (
    "exit", "reaped", "incomplete", "problem",
    "stdout_bytes", "stdout_sha256", "stdout_prefix", "stdout_truncated",
    "stderr_bytes", "stderr_sha256", "stderr_prefix", "stderr_truncated",
)
# An entity that carries a volume UUID must carry a well-formed one; a malformed value is
# a structure this census does not understand, never an entity without a UUID.
VOLUME_UUID_SHAPE = re.compile(
    r"^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$")
INFO_TIMEOUT_SECONDS = 10
DETACH_TIMEOUT_SECONDS = 30
HDIUTIL = "/usr/bin/hdiutil"
DISKUTIL = "/usr/sbin/diskutil"
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
    "BARGE_PROOF_ROUTE": "the scratch route the outer half verified, re-checked by the guarded half",
    "BARGE_PROOF_WALL_DEADLINE_NS": "the guard's absolute monotonic wall deadline, so staging spends it and never resets a clock",
    "OXIDE_PROOF_EVIDENCE": "the supervisor's evidence directory for the receipt and the image-custody indication; a sibling of the scratch directory inside the same monitored owned root",
    "OXIDE_TASK_SCRATCH_ROOT": "the supervisor's own scratch directory (one child of its monitored owned root); TMPDIR and the staged build live inside it",
    "DYLD_INSERT_LIBRARIES": "discarded from both halves: this proof injects no interposer",
    "HOME": "a private fixture home, so no real account state is read or written",
    "CARGO_HOME": "a private fixture Cargo home; the fixture build is the guarded proof-feature stage",
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
    {"id": "R13", "method": "case_coordinated_registry_releases_unrelated_and_keeps_live_reference",
     "claim": "a valid coordinated /4 registry keeps its live Oxide reference's root and releases an unrelated eligible root past a settlement naming that root's exact path",
     "carried_by": "a_coordinated_registry_releases_an_unrelated_root_and_keeps_its_live_reference"},
    {"id": "R14", "method": "case_malformed_coordinated_registry_refuses_the_batch",
     "claim": "a coordinated /4 registry without its settlement set refuses the whole batch and changes nothing",
     "carried_by": "a_malformed_coordinated_registry_refuses_the_whole_batch_and_changes_nothing"},
    {"id": "R15", "method": "case_coordinated_census_reports_the_registry",
     "claim": "the census reads the same coordinated /4 document as reported document facts, and its malformed twin stays unknown",
     "carried_by": "the_coordinated_census_reads_the_release_fixture_and_a_malformed_one_stays_unknown"},
    {"id": "R16", "method": "case_coordinated_adoption_group",
     "claim": "the emitted handoff — the removable peer registered, both /5 controls ready and their /4 image — releases an eligible root, while a mixed fence, an unequal transaction, a re-entered initialization and a one-sided image each keep the whole batch and change nothing",
     "carried_by": "a_one_sided_coordinated_adoption_keeps_the_whole_batch_and_changes_nothing"},
]

# The byte-level oracle for the identity record: its path inside the state directory.
IDENTITY_RELATIVE = os.path.join("roots", "{root_id}", "identity.json")
# The Oxide fixture and the synthetic root both live in the run root only.
RUN_ROOT_PREFIX = "release-proof-"

# Spelled apart so this list does not contain the tokens it forbids.
FORBIDDEN_TOKENS = ["-" + "force", "p" + "kill", "kill" + "all"]


def pool_state_document(fence, init=None):
    """One pool control state document exactly as Oxide serializes it. Pure."""
    document = {
        "version": 9,
        "next_id": 1,
        "next_use": 1,
        "tickets": [],
        "cleans": [],
        "records": [],
        "owned_outputs": fence,
        "recorded_accounting": "oxide.accounting/1",
        "eviction_accounting": "oxide.eviction-accounting/1",
        "slot_ceiling": "oxide.slots/256",
        "holding": "oxide.holding/1",
    }
    if init is not None:
        document["outputs_init"] = init
    return document


def registration_document():
    """Oxide's removable-pool registration, exactly its own field set. Pure.

    Registration is the coordinated pair membership: while it is here the
    reader must name the sibling control, and no fixture in this proof may rely
    on a directory alone to stand in for it.
    """
    return {
        "version": 2,
        "mount_point": "/Volumes/Builds",
        "volume_uuid": OXIDE_POOL_VOLUME_UUID,
        "container_uuid": OXIDE_POOL_VOLUME_UUID,
        "reserve_bytes": 1,
        "cap_bytes": 2,
        "quota_bytes": None,
        "capacity_bytes": None,
        "registered_ms": 1,
        "mode": "ephemeral",
    }


def ledger_document():
    """One pool library ledger with no slot, exactly as Oxide writes it. Pure."""
    return {
        "version": 1,
        "store_id": "0123456789abcdef0123456789abcdef",
        "cap_bytes": 1024,
        "next_slot": 1,
        "next_lease": 1,
        "slots": [],
    }


def registry_document(version, schema, revision, complete, created_by, rows, settled=None):
    """One output-registry document in the field set the version defines. Pure."""
    document = {
        "version": version,
        "schema": schema,
        "revision": revision,
        "complete": complete,
        "created_by": created_by,
        "rows": list(rows),
    }
    if settled is not None:
        document["settled"] = list(settled)
    return document


def coordinated_registry_document(rows, settled, complete, created_by):
    """The coordinated `/4` image, with the settlement set it always carries. Pure."""
    return registry_document(OXIDE_COORDINATED_REGISTRY_VERSION, 4, 2, complete,
                             created_by, rows, settled)


def legacy_registry_document(rows, complete):
    """The published `/2` image: no settled set, no deletion routes. Pure."""
    return registry_document(OXIDE_REGISTRY_VERSION, 2, 1, complete, "oxide", rows)


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


def volume_uuid(mount):
    """The mounted volume's own UUID from `diskutil`, or None when it is unreadable."""
    observed = subprocess.run([DISKUTIL, "info", "-plist", mount],
                              capture_output=True, timeout=INFO_TIMEOUT_SECONDS)
    if observed.returncode != 0:
        return None
    try:
        document = plistlib.loads(observed.stdout)
    except (ValueError, plistlib.InvalidFileException):
        return None
    uuid = document.get("VolumeUUID")
    return uuid if isinstance(uuid, str) and uuid else None


def scratch_route_facts(path=DESIGNATED_SCRATCH, mount=SCRATCH_VOLUME_MOUNT):
    """What the host says about the designated scratch: its path, owner, mode and volume.

    Read-only: one `lstat`, one real resolution and one bounded `diskutil` read of the
    mount's UUID. Nothing is created.
    """
    facts = {"path": path, "realpath": None, "volume_mount": mount,
             "volume_uuid": None, "owner_uid": None, "mode": None,
             "uid": os.getuid(), "present": False}
    try:
        info = os.lstat(path)
    except OSError:
        return facts
    facts["present"] = True
    facts["realpath"] = os.path.realpath(path)
    facts["owner_uid"] = info.st_uid
    facts["mode"] = info.st_mode & 0o777
    facts["volume_uuid"] = volume_uuid(mount)
    return facts


def scratch_route_problems(facts):
    """Why the designated scratch is not this proof's own volume path. Pure.

    `facts` is what `scratch_route_facts` observed (or a canned equivalent). A drive
    root, a home folder, a symlinked path, another account's directory, a
    group- or world-accessible scratch and another volume each refuse, and every
    refusal names the observed value, so a wrong host is never mistaken for a pass.
    """
    problems = []
    path = facts.get("path")
    if path != DESIGNATED_SCRATCH:
        problems.append(f"the designated scratch is {path!r}, not {DESIGNATED_SCRATCH!r}")
    if facts.get("present") is not True or facts.get("owner_uid") is None:
        problems.append(f"the designated scratch {path!r} is absent")
    if facts.get("realpath") != path:
        problems.append(f"the designated scratch resolves to {facts.get('realpath')!r}, so its path is not canonical")
    expected_parent = os.path.join(SCRATCH_VOLUME_MOUNT, "scratch")
    if not isinstance(path, str) or os.path.dirname(path) != expected_parent:
        problems.append(f"the designated scratch is not the barge scratch directly under {SCRATCH_VOLUME_MOUNT}")
    if facts.get("volume_mount") != SCRATCH_VOLUME_MOUNT:
        problems.append(f"the scratch mount is {facts.get('volume_mount')!r}, not {SCRATCH_VOLUME_MOUNT!r}")
    uuid = facts.get("volume_uuid")
    if not isinstance(uuid, str) or uuid.upper() != SCRATCH_VOLUME_UUID:
        problems.append(f"the scratch volume UUID is {uuid!r}, not the pinned {SCRATCH_VOLUME_UUID}")
    if facts.get("owner_uid") != facts.get("uid"):
        problems.append("the designated scratch is not owned by this account")
    mode = facts.get("mode")
    if not isinstance(mode, int) or mode & 0o077:
        problems.append(f"the designated scratch mode {mode!r} is not owner-only")
    return problems


def route_document(facts):
    """The verified scratch route as the document the guarded half re-checks. Pure."""
    return {"schema": ROUTE_SCHEMA, "designated_scratch": DESIGNATED_SCRATCH,
            "volume_mount": SCRATCH_VOLUME_MOUNT, "volume_uuid": SCRATCH_VOLUME_UUID,
            "observed": facts}


def route_refusals(document):
    """Why a carried route document does not name this proof's pinned scratch. Pure."""
    if not isinstance(document, dict):
        return ["the scratch route document is absent"]
    problems = []
    if document.get("schema") != ROUTE_SCHEMA:
        problems.append("the scratch route document is not this proof's schema")
    if document.get("designated_scratch") != DESIGNATED_SCRATCH:
        problems.append("the scratch route names another designated scratch")
    if document.get("volume_mount") != SCRATCH_VOLUME_MOUNT:
        problems.append("the scratch route names another mount")
    if document.get("volume_uuid") != SCRATCH_VOLUME_UUID:
        problems.append("the scratch route names another volume UUID")
    if scratch_route_problems(document.get("observed") or {"path": None}):
        problems.append("the scratch route's own observation is not this host's verified scratch")
    return problems


def stage_parent_problems(parent, owned):
    """Where a staged proof binary may live. Pure: names only, nothing is created.

    The staged copy belongs to this occurrence. Under the guard that is the run's own
    root and evidence directory, so the guard's monitored bytes and its custody cover
    it; the unguarded `stage` mode may only use the designated scratch. A home folder,
    a drive root, a relative path or a path reached through `..` refuses.
    """
    if not isinstance(parent, str) or not os.path.isabs(parent) or os.path.normpath(parent) != parent:
        return [f"the stage parent {parent!r} is not an absolute canonical path"]
    problems = []
    for path in owned.values():
        if isinstance(path, str) and os.path.isabs(path) and (
                parent == os.path.normpath(path)
                or parent.startswith(os.path.normpath(path) + os.sep)):
            break
    else:
        problems.append(f"the stage parent {parent} is outside every owned root it may use")
    if parent in (os.sep, SCRATCH_VOLUME_MOUNT, os.path.expanduser("~")):
        problems.append(f"the stage parent {parent} is a home folder or a drive root")
    if parent == DESIGNATED_SCRATCH:
        problems.append("the stage parent is the shared designated scratch, not this occurrence's own directory")
    return problems


def stage_directory(document):
    """The refusal-free summary of a staged build, or why it is not one. Pure."""
    problems = []
    if document.get("schema") != STAGE_SCHEMA:
        problems.append("the staged build is not this proof's schema")
    if document.get("features") != [PROOF_FEATURE]:
        problems.append(f"the staged build's features are {document.get('features')}, not [{PROOF_FEATURE!r}]")
    if not isinstance(document.get("sha256"), str) or len(document["sha256"]) != 64:
        problems.append("the staged build records no digest")
    return problems


def stage_environment_problems(environment):
    """The build's own environment: pinned, offline-capable and free of inherited flags. Pure."""
    problems = []
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "DYLD_INSERT_LIBRARIES"):
        if environment.get(name) is not None:
            problems.append(f"the stage environment still carries {name}")
    if environment.get("RUSTUP_AUTO_INSTALL") != "0":
        problems.append("the stage environment does not disable the toolchain auto-install")
    if environment.get("RUSTUP_TOOLCHAIN") != TOOLCHAIN_CHANNEL:
        problems.append(f"the stage environment does not pin the toolchain to {TOOLCHAIN_CHANNEL}")
    if environment.get("CARGO_NET_OFFLINE") != "true":
        problems.append("the stage environment does not ask Cargo to stay offline")
    for name in ("OXIDE_TASK_SCRATCH_ROOT", "OXIDE_PROOF_EVIDENCE", "OXIDE_TEST_HOME_FROM_ENV"):
        if environment.get(name) is not None:
            problems.append(f"the stage environment still carries the guard's {name} hook")
    if not isinstance(environment.get("TMPDIR"), str) or not environment["TMPDIR"]:
        problems.append("the stage environment has no temporary directory")
    for flag in ("--locked", "--offline", "--features", PROOF_FEATURE):
        if flag not in BUILD_COMMAND:
            problems.append(f"the build command does not name {flag}")
    return problems


def stage_environment(base, tmpdir):
    """The exact environment the guarded stage build runs with. Pure.

    It is an ordinary managed Oxide compilation: the guard's own scratch and evidence
    hooks are dropped so the build never nests a session inside this occurrence's
    fixture root, and only TMPDIR points at the guard so the compiler's temporary files
    stay monitored.
    """
    environment = {key: value for key, value in base.items()
                   if key not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "DYLD_INSERT_LIBRARIES",
                                  "RUSTUP_TOOLCHAIN", "RUSTUP_AUTO_INSTALL", "CARGO_NET_OFFLINE",
                                  "OXIDE_TASK_SCRATCH_ROOT", "OXIDE_PROOF_EVIDENCE",
                                  "OXIDE_TEST_HOME_FROM_ENV")}
    environment.update({"RUSTUP_TOOLCHAIN": TOOLCHAIN_CHANNEL, "RUSTUP_AUTO_INSTALL": "0",
                        "CARGO_NET_OFFLINE": "true", "TMPDIR": tmpdir})
    return environment


def custody_document(backing, root, evidence):
    """The guard's opt-in image-custody indication for this run's one backing. Pure.

    It is published before the backing exists, so an attach this run never finishes
    still leaves the guard able to see the backing it must not clean up under. The
    shape is the guard's own: exactly `version` and `backings`, one canonical path
    inside the guard's owned root or evidence directory.
    """
    if not isinstance(backing, str) or not os.path.isabs(backing) or os.path.normpath(backing) != backing:
        raise ValueError("the image backing is not an absolute canonical path")
    owned = [os.path.normpath(path) for path in (root, evidence)
             if isinstance(path, str) and os.path.isabs(path)]
    if not any(backing.startswith(path + os.sep) for path in owned):
        raise ValueError("the image backing is outside this run's owned root and evidence")
    return {"version": CUSTODY_VERSION, "backings": [backing]}


def is_safe_absolute_text(path):
    """Whether text is one canonical absolute path this census can compare. Pure.

    A relative path, a path with `.` or `..` components, a doubled or trailing separator
    or an embedded NUL is not something a census can relate to this run's backing, so it
    is a structure this reader does not understand rather than another image.
    """
    if not isinstance(path, str) or not path or "\0" in path:
        return False
    return os.path.isabs(path) and os.path.normpath(path) == path


def command_record(argv, completed=None, error=None):
    """One bounded external command's own record: argv, exit, reap and captured bytes. Pure.

    A command that did not complete — including one killed at its bound — is recorded as
    incomplete with whatever prefix of each stream it did produce; a timeout's partial
    captures are kept and their complete captured byte counts and digests are of exactly
    those bytes. `reaped` is true only for a command that returned, and `exit` is null
    otherwise. Each stream keeps a bounded raw prefix with an explicit truncation flag, so
    a deciding message is judgeable from the receipt.
    """
    def captured(value):
        if isinstance(value, bytes):
            return value
        if isinstance(value, str):
            return value.encode("utf-8", "replace")
        return b""

    stdout = b""
    stderr = b""
    problem = None
    exit_code = None
    reaped = False
    if completed is not None:
        exit_code = completed.returncode
        reaped = True
        stdout = captured(completed.stdout)
        stderr = captured(completed.stderr)
    else:
        if isinstance(error, subprocess.TimeoutExpired):
            stdout, stderr = captured(error.output), captured(error.stderr)
            problem = f"the command exceeded its {error.timeout} s bound"
        elif isinstance(error, BaseException):
            problem = f"the command did not complete: {error}"
        else:
            problem = "the command did not complete"
    return {
        "schema": COMMAND_SCHEMA,
        "argv": list(argv),
        "exit": exit_code,
        "reaped": reaped,
        "incomplete": not reaped,
        "problem": problem,
        "stdout_bytes": len(stdout),
        "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
        "stdout_prefix": stdout[:CENSUS_PREFIX_BYTES].decode("utf-8", "replace"),
        "stdout_truncated": len(stdout) > CENSUS_PREFIX_BYTES,
        "stderr_bytes": len(stderr),
        "stderr_sha256": hashlib.sha256(stderr).hexdigest(),
        "stderr_prefix": stderr[:CENSUS_PREFIX_BYTES].decode("utf-8", "replace"),
        "stderr_truncated": len(stderr) > CENSUS_PREFIX_BYTES,
    }


def parse_attachment_census(parsed, backing):
    """The parsed census for one backing, fail-closed. Pure.

    Returns `(problem, matched, entities, rows)`. `rows is None` is an unreadable
    census; `rows == []` is the only positive whole-backing absence and only a fully
    readable document with no image naming this backing establishes it. Every incomplete
    or malformed *relevant* structure — the top-level document, `images`, an
    `image-path`, the entity list itself, an entity's device, mount point or volume UUID,
    or a second image naming the same backing — is unreadable, never absence. Entities of
    other images are not interpreted.
    """
    if not is_safe_absolute_text(backing):
        return "the backing is not a canonical absolute path", False, [], None
    if not isinstance(parsed, dict):
        return "the census is not a dictionary", False, [], None
    images = parsed.get("images")
    if not isinstance(images, list):
        return "the census has no image list", False, [], None
    wanted = os.path.realpath(backing)
    matched, entities, seen = False, [], 0
    for image in images:
        if not isinstance(image, dict):
            return "an image entry is not a dictionary", matched, entities, None
        path = image.get("image-path")
        if not is_safe_absolute_text(path):
            return "an image entry has no canonical absolute image-path", matched, entities, None
        if os.path.realpath(path) != wanted:
            continue
        matched, seen = True, seen + 1
        if seen > 1:
            return "more than one image names this backing", matched, entities, None
        listed = image.get("system-entities")
        if not isinstance(listed, list) or not listed:
            return "the image's entity list is missing, empty or not a list", matched, entities, None
        for entity in listed:
            if not isinstance(entity, dict):
                return "an attachment entity is not a dictionary", matched, entities, None
            device = entity.get("dev-entry")
            if not isinstance(device, str) or not device:
                return "an attachment entity has no usable device", matched, entities, None
            mount = entity.get("mount-point")
            if mount is not None and (not isinstance(mount, str) or not mount):
                return "an attachment entity has an unusable mount point", matched, entities, None
            volume_uuid = entity.get("volume-uuid")
            if volume_uuid is not None and (
                    not isinstance(volume_uuid, str) or not VOLUME_UUID_SHAPE.match(volume_uuid)):
                return "an attachment entity has an unusable volume UUID", matched, entities, None
            entities.append({"device": device, "mount": mount, "volume_uuid": volume_uuid})
    return None, matched, entities, [(entity["device"], entity["mount"]) for entity in entities]


def attachment_census(backing, environment, runner=None):
    """One bounded, fail-closed attachment census for one backing. Pure but for the command.

    The report keeps the command's own argv, exit, reap, captured byte counts and digests
    and a bounded raw prefix, plus this backing's parsed entity identities and the exact
    deciding reason when it is unreadable. `rows is None` is unknown for every reason but
    a fully readable census that names no attachment of this backing.
    """
    argv = [HDIUTIL, "info", "-plist"]
    document = {
        "schema": ATTACHMENT_CENSUS_SCHEMA, "argv": argv, "exit": None, "reaped": False,
        "incomplete": True, "command": None,
        "stdout_bytes": 0, "stdout_sha256": None, "stdout_prefix": "",
        "stdout_truncated": False, "stderr_bytes": 0, "stderr_sha256": None,
        "stderr_prefix": "", "stderr_truncated": False,
        "unreadable": True, "problem": None,
        "backing": backing, "matched": False, "entities": [], "rows": None,
    }
    run_command = runner or (lambda command: subprocess.run(
        command, env=environment, capture_output=True, timeout=INFO_TIMEOUT_SECONDS))
    if not is_safe_absolute_text(backing):
        document["problem"] = "the backing is not a canonical absolute path"
        return document
    try:
        observed = run_command(argv)
    except (OSError, subprocess.SubprocessError) as error:
        record = command_record(argv, error=error)
        document["command"] = record
        for key in COMMAND_EVIDENCE_FIELDS:
            document[key] = record[key]
        document["problem"] = record["problem"] or "the attachment census did not complete"
        return document
    record = command_record(argv, observed)
    document["command"] = record
    for key in COMMAND_EVIDENCE_FIELDS:
        document[key] = record[key]
    if observed.returncode != 0:
        document["problem"] = f"the attachment census exited {observed.returncode}"
        return document
    try:
        parsed = plistlib.loads(observed.stdout or b"")
    except (ValueError, TypeError, plistlib.InvalidFileException):
        document["problem"] = "the attachment census is not a readable property list"
        return document
    problem, matched, entities, rows = parse_attachment_census(parsed, backing)
    document.update({"matched": matched, "entities": entities, "rows": rows})
    if problem is not None:
        document["problem"] = problem
        return document
    document["unreadable"] = False
    return document


def custody_target_problems(path, evidence, guard_evidence=None):
    """Why the custody indication is not at the guard's own evidence path. Pure.

    The indication must be the guard's `image-custody.json` inside the evidence directory
    the guard published for this occurrence; a path outside it, a symlink or a relative
    evidence directory refuses before any attach.
    """
    problems = []
    if not isinstance(evidence, str) or not os.path.isabs(evidence):
        problems.append("the guard's evidence directory is not an absolute path")
        return problems
    wanted = os.path.normpath(os.path.join(evidence, CUSTODY_NAME))
    if not isinstance(path, str) or os.path.normpath(path) != wanted:
        problems.append(f"the custody indication {path!r} is not the guard's {wanted!r}")
    if guard_evidence is not None:
        if not isinstance(guard_evidence, str) or not os.path.isabs(guard_evidence):
            problems.append("the guard's own evidence variable is not an absolute path")
        elif os.path.realpath(evidence) != os.path.realpath(guard_evidence):
            problems.append("the custody indication is not inside the guard's own evidence directory")
    if os.path.islink(path):
        problems.append("the custody indication path is a symbolic link")
    return problems


def attach_document(exit_code, reaped, rows, mount, device):
    """Why one attach is not positively this run's image at its own mount. Pure.

    An APFS image attaches as a container entity plus its mounted volume entity, exactly
    as the guard's own census and the reviewed space pattern read it, so the count of
    entities is not the identity: the decision is the one entity whose mount point is
    this run's unique mount, whose device is the one the attach child reported, and a
    refusal of any *other* entity that carries a mount point. A failed or unreaped
    child, an unreadable census, no mounted entity, a duplicate mount, a mismatched
    device or a second mounted entity all mean unknown custody: the backing is retained
    and never cleaned up.
    """
    if reaped is not True or exit_code != 0:
        return {"state": "unknown", "reason": f"the attach child did not succeed and reap (exit {exit_code})"}
    if rows is None:
        return {"state": "unknown", "reason": "the attachment census is unreadable"}
    mounted = [row for row in rows if row[1] == mount]
    if len(mounted) != 1:
        return {"state": "unknown",
                "reason": f"{len(mounted)} attachment entities name the mount {mount}"}
    if not isinstance(device, str) or not device:
        return {"state": "unknown", "reason": "the attach child reported no device for the mounted entity"}
    if mounted[0][0] != device:
        return {"state": "unknown", "reason": "the mounted device is not the one the attach reported"}
    elsewhere = [row for row in rows if row[1] is not None and row[1] != mount]
    if elsewhere:
        return {"state": "unknown",
                "reason": f"a second attachment entity is mounted at {elsewhere[0][1]}"}
    return {"state": "attached", "device": device, "mount": mount, "entities": len(rows)}


def detach_binding_problems(rows, mount, device, expected_uuid, observed_uuid):
    """Why the image may not be detached: its own bindings must agree first. Pure.

    The backing is one image with the container and volume entities the guard's census
    reads, so the binding is the one entity at this run's mount with this run's device,
    and no other mounted entity of the same backing. An extra mount of the owned image,
    a missing or duplicated binding, or another volume UUID refuses the detach.
    """
    problems = []
    if rows is None:
        problems.append("the pre-detach attachment census is unreadable")
        return problems
    bound = [row for row in rows if row == (device, mount)]
    if len(bound) != 1:
        problems.append(f"the pre-detach census names {len(bound)} entities for the owned {device} at {mount}")
    elsewhere = [row for row in rows if row[1] is not None and row != (device, mount)]
    if elsewhere:
        problems.append(f"the pre-detach census mounts a second entity of the backing at {elsewhere[0][1]}")
    if not isinstance(observed_uuid, str) or observed_uuid.upper() != str(expected_uuid).upper():
        problems.append(f"the mounted volume UUID {observed_uuid!r} is not the registered {expected_uuid!r}")
    return problems


def closure_document(purpose, exit_code, reaped, rows, mount_absent):
    """Whether one ordinary detach positively closed the owned backing. Pure.

    Closure needs all four: the ordinary child succeeded and was reaped, the census is
    readable and names no attachment for this backing at all (whole-backing absence,
    not merely an absent mount path), and the mount path is gone. A missing mount with
    the backing still attached, a non-zero or unreaped child and an unreadable census
    are each a retained, unqualified outcome with no retry.
    """
    document = {"purpose": purpose, "exit": exit_code, "reaped": bool(reaped),
                "mount_absent": mount_absent is True, "backing_attached": None,
                "closed": False, "reason": None, "custody": "retained"}
    if rows is None:
        document["reason"] = "the post-detach attachment census is unreadable"
        return document
    document["backing_attached"] = bool(rows)
    if reaped is not True or exit_code != 0:
        document["reason"] = f"the ordinary detach did not succeed and reap (exit {exit_code})"
        return document
    if rows:
        document["reason"] = (f"the backing is still attached ({len(rows)} entities, "
                              f"mounted at {rows[0][1]!r})")
        return document
    if mount_absent is not True:
        document["reason"] = "the whole backing is absent from the census but the mount path is still present"
        return document
    document["closed"] = True
    document["custody"] = "closed"
    return document


def guest_binding_problems(record, observed):
    """Check scoped instruction data against separately attributed observations.

    This comparison is not authentication, admission, or closure.  The host
    participant owns the record and independently verifies the registered VM;
    the guest cannot observe the host's Nucleus role or Parallels registration.
    """
    if not isinstance(record, dict) or not isinstance(observed, dict):
        return ["guest invocation binding or observations are missing"]
    required = {
        "task", "role", "run", "activation", "mode", "vm_uuid", "uid", "gid",
        "home", "mount", "volume_uuid", "scratch", "head", "tree", "lock_sha256",
        "guard_sha256", "toolchain", "target", "features", "command_sha256",
        "inputs_sha256", "nonce",
    }
    if set(record) != required:
        return ["guest invocation binding has missing or unexpected fields"]
    problems = []
    fixed = {"task": "0e9abb83-41c5-48b6-88e3-583c849adfef",
             "vm_uuid": GUEST_VM_UUID, "uid": GUEST_UID, "gid": GUEST_GID,
             "home": GUEST_HOME, "mount": GUEST_MOUNT,
             "volume_uuid": GUEST_VOLUME_UUID, "guard_sha256": SUPERVISOR_SHA256,
             "toolchain": "1.97.1", "target": "aarch64-apple-darwin",
             "features": [PROOF_FEATURE]}
    for key, expected in fixed.items():
        if record[key] != expected:
            problems.append(f"guest invocation {key} differs from its fixed value")
    if record["role"] not in ("author", "reviewer") or record["mode"] not in ("diagnostic", "counted"):
        problems.append("guest role or mode is unsupported")
    for key in ("run", "activation", "nonce"):
        if not isinstance(record[key], str) or not re.fullmatch(r"[A-Za-z0-9-]{16,128}", record[key]):
            problems.append(f"guest invocation {key} is malformed")
    for key, length in (("head", 40), ("tree", 40), ("lock_sha256", 64),
                        ("command_sha256", 64), ("inputs_sha256", 64)):
        if not isinstance(record[key], str) or not re.fullmatch(rf"[0-9a-f]{{{length}}}", record[key]):
            problems.append(f"guest invocation {key} is malformed")
    scratch = record["scratch"]
    if not isinstance(scratch, str) or os.path.dirname(scratch) != GUEST_PARENT or \
            not re.fullmatch(r"barge-proof-[A-Za-z0-9_-]{8,80}", os.path.basename(scratch)):
        problems.append("guest scratch is not a named private leaf of the Data parent")
    for key in ("uid", "gid", "home", "mount", "volume_uuid", "scratch", "head", "tree",
                "lock_sha256", "guard_sha256", "toolchain", "target", "features", "inputs_sha256"):
        if observed.get(key) != record[key]:
            problems.append(f"guest observation {key} differs from the invocation")
    for key in ("canonical", "nofollow", "owner_only", "same_volume", "reserve"):
        if observed.get(key) is not True:
            problems.append(f"guest route {key} is not established")
    return problems


def guest_receipt_problems(record, receipt, actual_digest, expected_digest):
    """Compare a complete returned receipt without treating correlation as authority."""
    if not isinstance(record, dict) or not isinstance(receipt, dict):
        return ["guest receipt or binding is missing"]
    problems = []
    if not isinstance(actual_digest, str) or not re.fullmatch(r"[0-9a-f]{64}", actual_digest) or \
            actual_digest != expected_digest:
        problems.append("complete guest receipt bytes do not match the expected digest")
    for key in ("nonce", "head", "tree", "lock_sha256", "guard_sha256", "inputs_sha256"):
        if not record.get(key) or receipt.get(key) != record[key]:
            problems.append(f"guest receipt {key} does not match the invocation")
    return problems


def guest_binding_controls():
    """Exercise the data comparisons; these do not exercise the live guest route."""
    record = {"task": "0e9abb83-41c5-48b6-88e3-583c849adfef", "role": "author",
              "run": "r" * 16, "activation": "a" * 16, "mode": "diagnostic",
              "vm_uuid": GUEST_VM_UUID, "uid": GUEST_UID, "gid": GUEST_GID,
              "home": GUEST_HOME, "mount": GUEST_MOUNT, "volume_uuid": GUEST_VOLUME_UUID,
              "scratch": GUEST_PARENT + "/barge-proof-12345678", "head": "a" * 40,
              "tree": "b" * 40, "lock_sha256": "c" * 64, "guard_sha256": SUPERVISOR_SHA256,
              "toolchain": "1.97.1", "target": "aarch64-apple-darwin",
              "features": [PROOF_FEATURE], "command_sha256": "d" * 64,
              "inputs_sha256": "e" * 64, "nonce": "n" * 16}
    observed = dict(record, canonical=True, nofollow=True, owner_only=True, same_volume=True, reserve=True)
    problems = []
    if guest_binding_problems(record, observed):
        problems.append("valid guest comparison was refused")
    for field in ("uid", "gid", "volume_uuid", "head", "tree", "lock_sha256", "guard_sha256",
                  "toolchain", "scratch", "inputs_sha256", "canonical", "nofollow", "owner_only",
                  "same_volume", "reserve"):
        if not guest_binding_problems(record, dict(observed, **{field: None})):
            problems.append(f"guest comparison accepted changed {field}")
    for field in record:
        reduced = dict(record)
        del reduced[field]
        if not guest_binding_problems(reduced, observed):
            problems.append(f"guest comparison accepted missing {field}")
    receipt = {key: record[key] for key in ("nonce", "head", "tree", "lock_sha256", "guard_sha256", "inputs_sha256")}
    if guest_receipt_problems(record, receipt, "f" * 64, "f" * 64):
        problems.append("matching guest receipt comparison was refused")
    if not guest_receipt_problems(record, dict(receipt, nonce="other"), "f" * 64, "f" * 64):
        problems.append("guest receipt comparison accepted a different nonce")
    if not guest_receipt_problems(record, receipt, "f" * 64, "0" * 64):
        problems.append("guest receipt comparison accepted different bytes")
    return problems


def inner_command(mode):
    """The argv the supervisor runs: this script and its mode. No artifact is named."""
    return [sys.executable, SCRIPT_PATH, "--inner", mode]


def supervisor_command(supervisor, mode):
    return [
        sys.executable, supervisor,
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", DESIGNATED_SCRATCH,
        "--", *inner_command(mode),
    ]


def argv_guard_problems():
    """Prove the outer command and the inner parser agree, and refuse an artifact. Pure."""
    problems = []
    for mode in ("counted", "diagnostic"):
        command = supervisor_command("/nonexistent/bounded-test.py", mode)
        if command[command.index("--") + 1:] != inner_command(mode):
            problems.append(f"the {mode} command does not end in this script's inner argv")
        inner = inner_command(mode)
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                parsed = argument_parser().parse_args(inner[2:])
            except SystemExit:
                problems.append(f"the {mode} inner argv does not parse with this script's parser")
                continue
        if parsed.inner != mode or parsed.mode is not None or hasattr(parsed, "binary"):
            problems.append(f"the {mode} inner argv parses to the wrong fields")
        for artifact in (["/nonexistent/barge-proof-binary"], ["--binary", "/nonexistent/barge"]):
            with contextlib.redirect_stderr(io.StringIO()):
                try:
                    argument_parser().parse_args(inner[2:] + artifact)
                except SystemExit:
                    continue
            problems.append(f"the {mode} inner argv accepts an artifact {artifact!r}")
        if any("--binary" in part or part.startswith("/nonexistent/")
               for part in command[command.index("--") + 1:]):
            problems.append(f"the {mode} guarded command names an artifact")
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


def staged_build_problems(binary, head, root):
    """Why a binary is not this occurrence's staged proof build of this head.

    Reads the manifest and the bytes. The staged path must lie inside the guard's own
    root, so a caller-supplied artifact from anywhere else is refused.
    """
    problems = []
    if not isinstance(binary, str) or not isinstance(root, str) or not os.path.isabs(binary):
        return ["the staged binary is not an absolute path inside an owned root"]
    owned = os.path.normpath(root)
    if not binary.startswith(owned + os.sep):
        problems.append(f"the binary {binary} is outside this occurrence's owned root {owned}")
    manifest_path = os.path.join(os.path.dirname(os.path.abspath(binary)), STAGE_MANIFEST)
    try:
        with open(manifest_path, encoding="utf-8") as handle:
            manifest = json.load(handle)
    except (OSError, ValueError) as error:
        return problems + [f"the staged manifest {manifest_path} is unreadable: {error}"]
    problems += stage_directory(manifest)
    if manifest.get("path") != os.path.abspath(binary):
        problems.append("the binary is not the path the staged manifest names")
    if manifest.get("head") != head:
        problems.append(f"the staged build is for head {manifest.get('head')}, not {head}")
    try:
        digest = sha256_file(binary)
    except OSError as error:
        return problems + [f"the staged binary is unreadable: {error}"]
    if manifest.get("sha256") != digest:
        problems.append("the staged binary's bytes no longer match the staged manifest's digest")
    return problems


def stage_proof_binary(parent, owned, deadline_ns=None, runner=None):
    """Build the proof-feature binary of HEAD through Oxide and stage a hashed copy.

    The copy goes in one exclusive directory under `parent`, which must be one of the
    `owned` roots this occurrence may stage inside, and the whole build spends only the
    wall the caller still holds: under the guard that is the supervisor's own 300 s
    bound, so staging and its waits are inside it rather than a clock of their own. The
    ordinary production build (target/debug/barge without the feature) is never the one
    staged, and no artifact from any other run is accepted. Raises Infrastructure when
    the build or the copy cannot be made.
    """
    problems = stage_parent_problems(parent, owned)
    if problems:
        raise Infrastructure("stage", "; ".join(problems))
    head = git_head()
    if not head:
        raise Infrastructure("stage", "HEAD is unreadable, so the staged build cannot be named")
    remaining = None
    if deadline_ns is not None:
        remaining = (deadline_ns - time.monotonic_ns()) / 1e9
        if remaining <= 0:
            raise Infrastructure("stage", "no guard wall is left to stage the proof build")
    environment = stage_environment(dict(os.environ), parent)
    environment_problems_found = stage_environment_problems(environment)
    if environment_problems_found:
        raise Infrastructure("stage", "; ".join(environment_problems_found))
    started = time.time()
    execute = runner or subprocess.run
    try:
        build = execute(BUILD_COMMAND, cwd=REPO_ROOT, env=environment,
                        capture_output=True, text=True, timeout=remaining)
    except subprocess.TimeoutExpired:
        raise Infrastructure("stage", "the guard's wall expired while staging the proof build") from None
    except OSError as error:
        raise Infrastructure("stage", f"the build could not start: {error}") from None
    if build.returncode != 0:
        raise Infrastructure("stage", f"oxide build exited {build.returncode}: {build.stderr[-400:]}")
    found = executable_from_build(build.stdout)
    if found is None:
        raise Infrastructure("stage", "the build reported no barge binary artifact")
    if found["features"] != [PROOF_FEATURE]:
        raise Infrastructure("stage", f"the barge artifact was built with features {found['features']}, not [{PROOF_FEATURE!r}]")
    directory = tempfile.mkdtemp(prefix=STAGE_PREFIX, dir=parent)
    staged = os.path.join(directory, "barge")
    shutil.copyfile(found["executable"], staged)
    os.chmod(staged, 0o755)
    manifest = {
        "schema": STAGE_SCHEMA,
        "head": head,
        "features": found["features"],
        "build_command": BUILD_COMMAND,
        "toolchain": TOOLCHAIN_CHANNEL,
        "offline": True,
        "source": found["executable"],
        "path": staged,
        "sha256": sha256_file(staged),
    }
    with open(os.path.join(directory, STAGE_MANIFEST), "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, indent=1)
        handle.write("\n")
    return manifest, {"seconds": round(time.time() - started, 3),
                      "bound_seconds": None if remaining is None else round(remaining, 3),
                      "environment": {key: environment[key] for key in
                                      ("RUSTUP_TOOLCHAIN", "RUSTUP_AUTO_INSTALL", "CARGO_NET_OFFLINE")}}


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
    failed = rollback_failure(same_record, same_marker, same_content, same_inode)
    if failed is not None:
        raise Fault("R11", failure_detail(failed, failure_summary({
            "identity_record": same_record, "marker": same_marker,
            "content": same_content, "inode": same_inode})))


def bounded_text(value, limit=FAILURE_TEXT_CHARS):
    """One free-text value, capped with an explicit marker. Pure."""
    if not isinstance(value, str) or len(value) <= limit:
        return value
    return value[:limit] + f"...[+{len(value) - limit} chars]"


def failure_summary(fields, rows=(), limit=ROW_EVIDENCE_CHARS):
    """A bounded failure record: the deciding fields first, then as many rows as fit. Pure.

    Scalars are never dropped, because a deciding field is useless if truncation can hide
    it. Free text is capped per value, rows are appended only while the whole record stays
    inside the existing reporting allowance, and the number omitted is stated.
    """
    summary = {key: bounded_text(value) for key, value in fields.items()}
    summary["rows_total"] = len(rows)
    summary["rows"] = []
    summary["rows_omitted"] = 0
    for index, row in enumerate(rows):
        rendered = {key: bounded_text(value) for key, value in row.items()}
        candidate = dict(summary, rows=summary["rows"] + [rendered])
        if len(json.dumps(candidate, sort_keys=True)) > limit - 100:
            summary["rows_omitted"] = len(rows) - index
            break
        summary["rows"].append(rendered)
    return summary


def first_failed_predicate(checks):
    """The first failing check's name, in order, short-circuiting exactly like `or`. Pure.

    `checks` is an ordered sequence of `(name, predicate)` pairs. Each predicate runs only
    when every earlier one held, so a failure record never causes an observation the
    original assertion would not have made.
    """
    for name, predicate in checks:
        if predicate():
            return name
    return None


def failure_detail(predicate, summary):
    """The one-line failure record an oracle raises: predicate first, bounded fields after."""
    return f"{predicate} failed: {json.dumps(summary, sort_keys=True)}"


ROW_ID_LIMIT = 32
ROW_ID_CHARS = 400


def bounded_ids(values, count_limit=ROW_ID_LIMIT, char_limit=ROW_ID_CHARS):
    """At most `count_limit` identifiers inside a `char_limit` budget, omission stated. Pure.

    A returned root-id list is one member of a bounded failure record, so it needs its own
    bound: an unbounded list would blow the record's allowance on its own, however well the
    per-row entries are capped.
    """
    kept, used = [], 0
    for value in values:
        rendered = bounded_text(value, 120) if isinstance(value, str) else value
        size = len(json.dumps(rendered)) + 1
        if len(kept) >= count_limit or used + size > char_limit:
            break
        kept.append(rendered)
        used += size
    return kept, max(0, len(values) - len(kept))


def stated_rows(data):
    """A returned document's `rows` member as usable row objects, plus what it stated. Pure.

    A member the document did not state is `unobserved`; a member of the wrong type is
    `unusable` and contributes no rows, because iterating it would raise before the
    command's own independently captured evidence could be published. A list whose
    members are not objects keeps the usable ones and states how many were dropped.
    """
    if "rows" not in data:
        return "unobserved", 0, []
    raw = data["rows"]
    if not isinstance(raw, list):
        return f"unusable ({type(raw).__name__})", "unobserved", []
    rows = [row for row in raw if isinstance(row, dict)]
    dropped = len(raw) - len(rows)
    return ("usable" if not dropped else f"list with {dropped} non-object members",
            len(raw), rows)


def barge_document_summary(document):
    """The deciding fields one returned barge document states, each bounded. Pure.

    Only what the document itself says is reported: a member it does not state is
    `unobserved`, never guessed, and the raw document is not re-encoded here. Counters,
    digests, root ids and each returned outcome/reason are kept as their own bounded
    fields, so an unrelated leading member can never consume the record. An unusable
    member shape is stated as such instead of being iterated, so a syntactically valid
    but unusable result can never raise here — and so can never discard the command
    record this summary is published beside.
    """
    if not isinstance(document, dict):
        return {"document": f"unusable ({type(document).__name__})"}
    error = document.get("error") if isinstance(document.get("error"), dict) else {}
    data = document.get("data") if isinstance(document.get("data"), dict) else {}
    plan = data.get("plan") if isinstance(data.get("plan"), dict) else {}
    rows_shape, rows_stated, rows = stated_rows(data)
    plan_rows = plan.get("rows") if isinstance(plan.get("rows"), dict) else {}
    listed = plan_rows.get("rows") if isinstance(plan_rows.get("rows"), list) else []
    root_ids, ids_omitted = bounded_ids([row.get("root_id") for row in listed
                                         if isinstance(row, dict)])

    def stated(name, value="unobserved"):
        return data[name] if name in data else value

    fields = {
        "schema": document.get("schema", "unobserved"),
        "command": document.get("command", "unobserved"),
        "ok": document.get("ok", "unobserved"),
        "error_kind": error.get("kind", "unobserved"),
        "error_message": error.get("message", "unobserved"),
        "presented_plan_digest": stated("plan_digest"),
        "fresh_plan_digest": stated("fresh_plan_digest"),
        "released_count": stated("released_count"),
        "kept_count": stated("kept_count"),
        "stopped": stated("stopped"),
        "replay_pending": stated("replay_pending"),
        "plan_eligible_count": plan_rows.get("eligible_count", "unobserved"),
        "plan_examined_count": plan_rows.get("examined_count", "unobserved"),
        "plan_root_ids": root_ids,
        "plan_root_ids_omitted": ids_omitted,
        "rows_shape": rows_shape,
        "rows_stated": rows_stated,
    }
    return failure_summary(fields, [{"root_id": row.get("root_id"),
                                     "outcome": row.get("outcome"),
                                     "reason": row.get("reason")} for row in rows])


def stream_prefix_evidence(command, name):
    """One captured stream's bounded prefix with its honest capture facts. Pure.

    A capture the command record marked truncated keeps that flag in the line itself, and
    the further bound applied here states exactly how many of the captured characters are
    shown, so the receipt can never imply it published a whole stream. A stream the record
    did not capture — an unrunnable command captures nothing — says so rather than
    rendering an empty string that could be mistaken for a stream that was empty.
    """
    raw = command.get(name)
    if not isinstance(raw, str):
        return f"{name} (capture unobserved)"
    flag = command.get(name.replace("_prefix", "_truncated"))
    facts = "capture_truncated=" + (str(flag).lower() if isinstance(flag, bool) else "unobserved")
    shown = bounded_text(raw, FAILURE_TEXT_CHARS)
    if len(raw) > FAILURE_TEXT_CHARS:
        facts += f"; shown {FAILURE_TEXT_CHARS} of {len(raw)} captured chars"
    return f"{name} ({facts}) " + (shown or "<empty>")


def barge_fault_detail(argv, expect, command, document, because, observed_exit=None):
    """One barge fault: the command's captures, then the deciding fields, then prefixes. Pure.

    `command` is a [`command_record`] of exactly the captured bytes. The deciding summary
    of the returned document is bounded independently of that command record, and the raw
    stream prefixes are published last and unconditionally, so neither a hostile leading
    member nor a long capture can hide a counter, digest, root id or reason, and a fault
    that parsed no usable document still keeps the bytes that explain it. A summary that
    cannot render — a last resort, since [`barge_document_summary`] itself states unusable
    shapes — degrades to an unobserved document instead of discarding the command record.
    Facts the command or document did not state stay `unobserved`.
    """
    command_fields = {
        "argv": command.get("argv"),
        "expected_exit": expect,
        "observed_exit": observed_exit if observed_exit is not None else command.get("exit"),
        "reaped": command.get("reaped"),
        "incomplete": command.get("incomplete"),
        "problem": command.get("problem"),
        "stdout_bytes": command.get("stdout_bytes"),
        "stdout_sha256": command.get("stdout_sha256"),
        "stdout_truncated": command.get("stdout_truncated"),
        "stderr_bytes": command.get("stderr_bytes"),
        "stderr_sha256": command.get("stderr_sha256"),
        "stderr_truncated": command.get("stderr_truncated"),
    }
    parts = [f"barge {' '.join(argv)} {because}",
             "command " + json.dumps(command_fields, sort_keys=True)]
    if document is None:
        parts.append("deciding unobserved (no usable document)")
    else:
        try:
            rendered = json.dumps(barge_document_summary(document), sort_keys=True)
        except Exception as error:  # last resort: never lose the command's own evidence
            rendered = json.dumps({"document": f"unusable ({type(error).__name__})"})
        parts.append("deciding " + rendered)
    parts.append(stream_prefix_evidence(command, "stdout_prefix"))
    parts.append(stream_prefix_evidence(command, "stderr_prefix"))
    return bounded_text("; ".join(parts), BARGE_FAULT_DETAIL_CHARS)


def run_report_summary(document, expected_root_id, observations=None):
    """The decisive fields one `barge.prune.run/1` body already returned. Pure.

    `observations` carries predicates the original assertion actually reached; a
    filesystem fact the branch never reached is reported as `unobserved`, never guessed.
    """
    observations = observations or {}
    rows = [row for row in (document.get("rows") or []) if isinstance(row, dict)]
    returned_ids, ids_omitted = bounded_ids([row.get("root_id") for row in rows])
    fields = {
        "released_count": document.get("released_count"),
        "kept_count": document.get("kept_count"),
        "stopped": document.get("stopped"),
        "replay_pending": document.get("replay_pending"),
        "expected_root_id": expected_root_id,
        "row_root_ids": returned_ids,
        "row_root_ids_omitted": ids_omitted,
        "path_present": observations.get("path_present", "unobserved"),
    }
    return failure_summary(fields, [{"root_id": row.get("root_id"),
                                     "outcome": row.get("outcome"),
                                     "reason": row.get("reason")} for row in rows])


def released_after_replay_failure(document, expected_root_id, path_present):
    """Which post-replay predicate fails, in the original order, plus what it observed. Pure.

    `path_present` is a thunk: the filesystem is read exactly once and only when the
    earlier predicates held, exactly as the original `or` chain did.
    """
    observations = {}

    def observe_path():
        observations["path_present"] = bool(path_present())
        return observations["path_present"]

    failed = first_failed_predicate([
        ("released_count", lambda: document["released_count"] != 1),
        ("path_present", observe_path),
        ("row_count", lambda: len(document["rows"]) != 1),
        ("root_id", lambda: document["rows"][0]["root_id"] != expected_root_id),
    ])
    return failed, observations


def root_kept_failure(document, expected_root_id, path_present):
    """Which "this root must still be here" predicate fails, in order. Keep polarity. Pure.

    The original assertions were `released_count != 0 or not lexists(path)`: the count is
    decided first and the filesystem is read once, only when it held. The path predicate
    fails when the path is gone.
    """
    observations = {}

    def observe_path():
        observations["path_present"] = bool(path_present())
        return not observations["path_present"]

    failed = first_failed_predicate([
        ("released_count", lambda: document["released_count"] != 0),
        ("path_present", observe_path),
    ])
    return failed, observations


def root_released_failure(document, expected_root_id, path_present, expected_released=1):
    """Which "this root must be gone" predicate fails, in order. Release polarity. Pure.

    The mirror of `root_kept_failure` as its own function, so a release assertion can never
    pick up keep polarity by passing a flag: the count is decided first and the filesystem
    is read once, only when it held, and this predicate fails when the path is still there.
    """
    observations = {}

    def observe_path():
        observations["path_present"] = bool(path_present())
        return observations["path_present"]

    failed = first_failed_predicate([
        ("released_count", lambda: document["released_count"] != expected_released),
        ("path_present", observe_path),
    ])
    return failed, observations


def replay_settlement_failure(document, pending_intents):
    """Which replay predicate fails, in order; intents are read only when reached. Pure."""
    return first_failed_predicate([
        ("replay_pending", lambda: document["replay_pending"] != 0),
        ("pending_intents", lambda: bool(pending_intents())),
    ])


def rollback_failure(same_record, same_marker, same_content, same_inode):
    """Which rollback observation differs, in the original order. Pure."""
    return first_failed_predicate([
        ("identity_record", lambda: not same_record),
        ("marker", lambda: not same_marker),
        ("content", lambda: not same_content),
        ("inode", lambda: not same_inode),
    ])


def row_deciding_summary(row):
    """The fields that decide one plan row's eligibility, in a fixed order. Pure.

    A plan row is a sorted-key document, so `absence` sorts first and can consume a
    whole truncation by itself. The frozen oracle decides on `eligible` alone; this
    summary only makes a failing row judgeable from the receipt after the run root is
    gone, by recording the row's own id, each gate's own verdict, its reason and the
    first blocker. `root_id` is here, not only in the raw row, so a truncated row can
    never hide which root the record is about.
    """
    identity = row.get("identity") or {}
    idle = row.get("idle") or {}
    absence = row.get("absence") or {}
    references = [reference for reference in (row.get("references") or [])
                  if isinstance(reference, dict)]
    return {
        "root_id": row.get("root_id"),
        "eligible": row.get("eligible"),
        "blocked_by": row.get("blocked_by"),
        "reason": row.get("reason"),
        "identity_verdict": identity.get("verdict"),
        "identity_reason": identity.get("reason"),
        "custody": row.get("custody"),
        "custody_reason": row.get("custody_reason"),
        "last_activity_ms": idle.get("last_activity_ms"),
        "idle_reason": idle.get("reason"),
        "age_waived": row.get("age_waived"),
        "references_complete": row.get("references_complete"),
        "references_current": sorted(str(reference.get("source")) for reference in references
                                     if reference.get("current")),
        "absence_verdict": absence.get("verdict"),
        "absence_reason": absence.get("reason"),
    }


def row_failure(predicate, row, **extra):
    """One plan-row oracle failure: every deciding field, then the bounded row. Pure.

    A failed native occurrence is judged after its run root is gone, so a record
    that names only the first blocker is not enough. Every deciding field is
    published — the row's own id, each gate's verdict and its reason, the custody
    verdict and its reason included — and each free-text value is capped **on its
    own**, inside `ROW_FAILURE_FIELD_CHARS`, so one long reason cannot hide the
    fields that follow it in sorted order. The raw row then rides inside
    `ROW_EVIDENCE_CHARS`, and any caller comparison field follows it.
    `predicate` names the one check that failed.
    """
    deciding = {
        name: bounded_text(value, ROW_FAILURE_FIELD_CHARS)
        for name, value in row_deciding_summary(row).items()
    }
    rendered = bounded_text(json.dumps(row, sort_keys=True), ROW_EVIDENCE_CHARS)
    fields = "".join(
        f"; {name} "
        f"{bounded_text(value if isinstance(value, str) else json.dumps(value, sort_keys=True))}"
        for name, value in extra.items()
    )
    return f"{predicate} failed: deciding {json.dumps(deciding, sort_keys=True)}; row {rendered}{fields}"


class Proof:
    """One counted or diagnostic run: one image, one private HOME and state, many areas."""

    def __init__(self, binary, receipt_path, diagnostic, build, evidence=None, route=None):
        self.binary = binary
        self.receipt_path = receipt_path
        self.evidence = evidence or os.path.dirname(os.path.abspath(receipt_path))
        self.diagnostic = diagnostic
        self.build = build
        self.route = route or {}
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
        self.remount = None
        self.custody = None
        self.attach_census = None
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
        for name in (HDIUTIL, DISKUTIL):
            if not os.path.isfile(name):
                raise Infrastructure("host", f"{name} is not available")
        self.image = os.path.join(self.root, "release-volume.sparseimage")
        # The guard's opt-in custody indication is published before the backing exists,
        # so an attach interrupted anywhere below still leaves the backing visible to
        # the guard's own cleanup, which then keeps the root instead of removing it.
        self.publish_custody()
        run([HDIUTIL, "create", *CREATE_IMAGE_OPTIONS, self.image], self.env)
        self.attach(first=True)
        self.write_registry([self.volume_record()])
        code, _ = self.barge("describe", expect=0)
        if code != 0:
            raise Fault("premise", "the binary under test does not run `describe`")

    def publish_custody(self):
        """Publish the guard's image-custody indication: one bounded atomic document.

        The indication must land at the guard's own evidence path for this occurrence,
        which is verified before anything is written and therefore before any attach.
        """
        guard_evidence = os.environ.get("OXIDE_PROOF_EVIDENCE")
        evidence = guard_evidence or self.evidence
        document = custody_document(self.image, self.root, evidence)
        path = os.path.join(evidence, CUSTODY_NAME)
        problems = custody_target_problems(path, evidence, guard_evidence)
        if problems:
            raise Infrastructure("custody", "; ".join(problems))
        self.evidence = evidence
        temporary = path + ".tmp"
        try:
            descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                json.dump(document, handle, separators=(",", ":"))
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, path)
        except OSError as error:
            raise Infrastructure("custody", f"the image-custody indication cannot be published: {error}") from error
        self.custody = {"path": path, "published_before_backing": not os.path.lexists(self.image),
                        "version": document["version"], "backings": document["backings"]}
        if not self.custody["published_before_backing"]:
            raise Infrastructure("custody", "the custody indication was published after the backing existed")

    def attach(self, first):
        """Attach the owned image once and require a positive identity for it.

        A failed, partial or unreadable attach is unknown custody: the mount, backing
        and evidence are retained, the guard's custody indication keeps the root, and
        nothing is detached, repaired or retried.
        """
        name = f"barge-release-proof-{os.getpid()}-{int(time.time()) % 100000}-{'a' if first else 'b'}"
        mount = f"/Volumes/{name}"
        if os.path.realpath(self.image) != self.image or os.path.lexists(mount):
            raise Infrastructure("attach", "the backing is not canonical or the mount already exists")
        self.mount = mount
        self.mount_history.append(mount)
        self.attached = True  # An interrupted attach has unknown custody, not absence.
        try:
            spawn = subprocess.run([HDIUTIL, "attach", self.image, "-mountpoint", mount,
                                    *ATTACH_OPTIONS, "-plist"], env=self.env,
                                   capture_output=True, timeout=120)
        except (OSError, subprocess.SubprocessError) as error:
            raise Infrastructure("attach", f"the attach child did not complete: {error}") from error
        device = None
        try:
            entities = plistlib.loads(spawn.stdout)["system-entities"]
            mounted = [row for row in entities if row.get("mount-point") == mount]
            if len(mounted) == 1 and spawn.returncode == 0:
                device = mounted[0].get("dev-entry")
        except (ValueError, KeyError, TypeError, plistlib.InvalidFileException):
            device = None
        census = attachment_census(self.image, self.env)
        self.attach_census = census
        document = attach_document(spawn.returncode, True, census["rows"], mount, device)
        if document["state"] != "attached":
            reason = document["reason"]
            if census.get("problem"):
                reason = f"{reason}: {census['problem']}"
            raise Infrastructure("attach", reason)
        info = run([DISKUTIL, "info", "-plist", mount], self.env)
        parsed = plistlib.loads(info.stdout.encode())
        uuid = parsed.get(VOLUME_UUID_KEY)
        observed_device = parsed.get("DeviceIdentifier")
        if not uuid or not observed_device:
            raise Infrastructure("attach", "the owned image did not report its UUID and device")
        if not device.endswith(observed_device):
            raise Infrastructure("attach", "the attached entity and the mounted volume report different devices")
        text = run([DISKUTIL, "info", mount], self.env).stdout
        state = ownership_state(text)
        if first:
            self.uuid = uuid
            self.device = device
            self.ownership = {"attach_argv": list(ATTACH_OPTIONS), "text_state": state}
        else:
            if uuid != self.uuid:
                raise Infrastructure("attach", "the reattached image reports a different volume UUID")
            self.device_after_remount = device
        if state != "enabled":
            raise Infrastructure("attach", f"the image does not state `Owners: Enabled` (reads {state!r})")
        return {"state": "attached", "mount": mount, "device": device, "volume_uuid": uuid,
                "backing": self.image, "ownership": state, "first": first}

    def detach(self, purpose):
        """One ordinary detach, authorized by a fresh rebind census and closed positively.

        Each call re-reads the image's own bindings (backing, mount, device and volume
        UUID) before it detaches, so the intermediate R11 detach and the final one
        never borrow each other's authority. Never forced, never retried, and never
        accepted on a missing mount alone: the whole backing must be absent from the
        attachment census.
        """
        if not self.attached or self.mount is None or self.uuid is None or self.device is None:
            return {"purpose": purpose, "exit": None, "reaped": False, "mount_absent": False,
                    "backing_attached": None, "closed": False, "custody": "retained",
                    "reason": "no positively recorded attachment to detach"}
        before = attachment_census(self.image, self.env)
        problems = detach_binding_problems(before["rows"], self.mount,
                                           self.device, self.uuid, volume_uuid(self.mount))
        if problems:
            return {"purpose": purpose, "exit": None, "reaped": False, "mount_absent": False,
                    "backing_attached": None, "closed": False, "custody": "unknown",
                    "reason": "; ".join(problems), "command": None,
                    "pre_census": before, "post_census": None}
        argv = [HDIUTIL, "detach", self.mount]
        try:
            observed = subprocess.run(argv, env=self.env, capture_output=True,
                                      timeout=DETACH_TIMEOUT_SECONDS)
        except (OSError, subprocess.SubprocessError) as error:
            exit_code, reaped, command = None, False, command_record(argv, error=error)
        else:
            exit_code, reaped = observed.returncode, True
            command = command_record(argv, observed)
        after = attachment_census(self.image, self.env)
        document = closure_document(purpose, exit_code, reaped, after["rows"],
                                    not os.path.lexists(self.mount))
        if after["rows"] is None and after.get("problem"):
            document["reason"] = f"{document['reason']}: {after['problem']}"
        if command["incomplete"]:
            document["reason"] = f"{document['reason']}; the detach command did not complete: {command['problem']}"
        document["command"] = command
        document["pre_census"] = before
        document["post_census"] = after
        if document["closed"]:
            self.attached = False
        return document

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
        """Run the binary with `--json`; return (exit, document). `expect` is checked.

        The child's streams are captured as bytes, so a fault record's digests and prefixes
        are of exactly those bytes. A non-JSON result, an unexpected exit, a timeout or a
        command that cannot run keeps the command's own bounded evidence and the returned
        document's separately bounded deciding fields — never one 300-character prefix.
        """
        argv = [self.binary, *args, "--json"]
        try:
            result = subprocess.run(argv, env=self.env, cwd=self.root,
                                    capture_output=True, timeout=timeout)
        except (OSError, subprocess.SubprocessError) as error:
            raise Fault("barge", barge_fault_detail(
                argv, expect, command_record(argv, error=error), None,
                f"did not complete: {error}")) from None
        command = command_record(argv, result)
        try:
            document = json.loads((result.stdout or b"").decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            raise Fault("barge", barge_fault_detail(
                argv, expect, command, None,
                f"printed no usable JSON (exit {result.returncode})")) from None
        if expect is not None and result.returncode != expect:
            raise Fault("barge", barge_fault_detail(
                argv, expect, command, document,
                f"exited {result.returncode}, expected {expect}",
                observed_exit=result.returncode)) from None
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

    def pool_controls(self):
        """Both control directories the emitted pair writes, under the private HOME."""
        control = os.path.join(self.home, OXIDE_RELATIVE_CONTROL)
        return [control, os.path.join(control, "removable", "oxide-managed")]

    def clear_pool_controls(self):
        """Remove the adoption documents an earlier case wrote under this HOME."""
        for control in self.pool_controls():
            for name in ("state.json",):
                with contextlib.suppress(FileNotFoundError):
                    os.unlink(os.path.join(control, name))
            with contextlib.suppress(FileNotFoundError):
                os.unlink(os.path.join(os.path.dirname(control), "ledger.json"))
        with contextlib.suppress(FileNotFoundError):
            os.unlink(os.path.join(self.home, OXIDE_RELATIVE_CONTROL, OXIDE_REGISTRATION_NAME))

    def peer_registration(self):
        """Publish Oxide's registration of the removable pool under the private HOME."""
        self.publish(os.path.join(self.home, OXIDE_RELATIVE_CONTROL, OXIDE_REGISTRATION_NAME),
                     registration_document())

    def publish(self, path, document):
        """Write one fixture JSON document at `path`.

        This is the fixture's only write primitive: the document building — the
        paired `/5` controls, their ledgers and the `/4` image — is the pure
        builders above, so an effect-free control can drive every real writer
        while only this write is scripted.
        """
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(document, handle)

    def pool_state(self, control, fence, init):
        """One pool control state and its library ledger, exactly as Oxide writes them."""
        self.publish(os.path.join(control, "state.json"), pool_state_document(fence, init))
        self.publish(os.path.join(os.path.dirname(control), "ledger.json"), ledger_document())

    def write_pair(self, internal, removable):
        """Write both pool controls; each side is `(fence, initialization-or-None)`."""
        for control, side in zip(self.pool_controls(), (internal, removable)):
            self.pool_state(control, *side)

    @staticmethod
    def ready(transaction=OXIDE_TRANSACTION):
        """Oxide's completed initialization record."""
        return {"ready": {"transaction": transaction, "ready_ms": 1}}

    @staticmethod
    def initializing(transaction=OXIDE_TRANSACTION):
        """Oxide's mid-adoption initialization record."""
        return {"initializing": {"transaction": transaction, "started_ms": 1}}

    def coordinated_adoption(self, rows=(), settled=()):
        """The exact emitted handoff: the peer registered, both `/5` controls ready,
        then their `/4` image."""
        self.peer_registration()
        self.write_pair((OXIDE_COORDINATED_FENCE, self.ready()),
                        (OXIDE_COORDINATED_FENCE, self.ready()))
        self.coordinated_registry(list(rows), list(settled))

    def legacy_registry(self, rows, complete=True):
        """Publish one legacy `/2` output-registry document, leaving the controls alone."""
        self.publish(os.path.join(self.home, OXIDE_RELATIVE_REGISTRY),
                     legacy_registry_document(rows, complete))

    def oxide_registry(self, rows, complete=True):
        """Publish one legacy `/2` registry with no coordinated controls beside it.

        Every case runs against one synthetic home, so the adoption documents a
        previous case wrote are cleared here: a legacy image is read against
        controls that establish no coordinated handoff.
        """
        self.clear_pool_controls()
        self.legacy_registry(rows, complete=complete)

    def coordinated_registry(self, rows, settled, complete=True, path=None,
                             created_by=OXIDE_TRANSACTION):
        """Publish one coordinated (/4) output-registry document.

        The default path is the release reader's resolution under the private
        HOME; `path` publishes the identical document where another reader —
        the census — resolves the registry, so one fixture can serve both.
        `created_by` is the initialization transaction that created the image;
        the emitted handoff uses the transaction both controls record ready.
        """
        path = path or os.path.join(self.home, OXIDE_RELATIVE_REGISTRY)
        self.publish(path, coordinated_registry_document(rows, settled, complete, created_by))

    def settlement(self, root):
        """One settlement entry, exactly Oxide's serialized `Settlement` field set."""
        return {
            "caller": "oxide.outputs",
            "key": "a" * 32,
            "checkout_id": OXIDE_CHECKOUT_ID,
            "root": root,
            "nonce": "b" * 32,
            "outcome": "deleted",
            "terminal_ms": 1,
            "request_sha256": "d" * 64,
        }

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
            raise Fault(
                case,
                f"expected eligible={expect_eligible}; "
                f"deciding {json.dumps(row_deciding_summary(row), sort_keys=True)}; "
                f"row {json.dumps(row)[:ROW_EVIDENCE_CHARS]}",
            )
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
            raise Fault("R1", failure_detail(
                "released_count", run_report_summary(run_doc["data"], root["root_id"])))
        present = os.path.lexists(root["path"])
        if present:
            raise Fault("R1", failure_detail("path_present", run_report_summary(
                run_doc["data"], root["root_id"], {"path_present": present})))
        if self.pending_intents():
            raise Fault("R1", failure_detail("pending_intents", run_report_summary(
                run_doc["data"], root["root_id"])))
        settled = self.settled_intents()
        observations = {"settled_intents": len(settled), "expected_settled_intents": 1}

        def settled_identity_missing():
            observations["settled_intent_identity_present"] = settled[0].get("identity") is not None
            return not observations["settled_intent_identity_present"]

        failed = first_failed_predicate([
            ("settled_intent_count", lambda: observations["settled_intents"] != 1),
            ("settled_intent_identity", settled_identity_missing),
        ])
        if failed is not None:
            raise Fault("R1", failure_detail(failed, failure_summary(observations)))
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
        failed, observations = root_kept_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R2", failure_detail(
                failed, run_report_summary(run_doc["data"], root["root_id"], observations)))
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
        failed, observations = root_kept_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R3", failure_detail(
                failed, run_report_summary(run_doc["data"], root["root_id"], observations)))
        return {"root": root["root_id"], "blocked_by": row["blocked_by"],
                "counterfactual_of": "R1", "flipped_condition": "custody"}

    def case_named_root_waives_only_the_age_gate(self):
        area = self.fresh_area("c04", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=False)
        _p, young, _f = self.plan_and_run("R4-young", area, root, expect_eligible=False)
        # The plan omits `age_waived` when it is false, so an absent key is an unwaived row.
        failed = first_failed_predicate([
            ("blocked_by", lambda: young["blocked_by"] != "policy"),
            ("age_waived", lambda: bool(young.get("age_waived"))),
        ])
        if failed is not None:
            raise Fault("R4", row_failure(failed, young))
        preview, row, plan_file = self.plan_and_run(
            "R4-named", area, root, named=[root["root_id"]], expect_eligible=True
        )
        failed = first_failed_predicate([
            ("age_waived", lambda: not row["age_waived"]),
            ("named_roots",
             lambda: preview["data"]["plan"]["named_roots"] != [root["root_id"]]),
        ])
        if failed is not None:
            raise Fault("R4", row_failure(
                failed, row,
                named_roots=preview["data"]["plan"]["named_roots"],
                expected_named_roots=[root["root_id"]]))
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        failed, observations = root_released_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R4", failure_detail(
                failed, run_report_summary(run_doc["data"], root["root_id"], observations)))
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
        failed, observations = root_kept_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R5", failure_detail(
                failed, run_report_summary(run_doc["data"], root["root_id"], observations)))
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
        absence = row.get("absence") or {}
        failed = first_failed_predicate([
            ("blocked_by", lambda: row["blocked_by"] != "absence"),
            ("absence_verdict", lambda: absence.get("verdict") != "present"),
        ])
        if failed is not None:
            raise Fault("R6", row_failure(failed, row))
        os.rmdir(checkout)
        preview, row, plan_file = self.plan_and_run("R6-absent", area, root, expect_eligible=True)
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        if run_doc["data"]["released_count"] != 1:
            raise Fault("R6", failure_detail(
                "released_count", run_report_summary(run_doc["data"], root["root_id"])))
        present = os.path.lexists(root["path"])
        if present:
            raise Fault("R6", failure_detail("path_present", run_report_summary(
                run_doc["data"], root["root_id"], {"path_present": present})))
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
        failed, observations = root_kept_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R8", failure_detail(
                failed, run_report_summary(run_doc["data"], root["root_id"], observations)))
        return {"root": root["root_id"], "blocked_by": row["blocked_by"],
                "counterfactual_of": "R1", "flipped_condition": "marker kind"}

    def case_changed_digest_refuses_and_changes_nothing(self):
        area = self.fresh_area("c09", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        preview, row, plan_file = self.plan_and_run("R9", area, root, expect_eligible=True)
        refusal = self.refused(plan_file, "0" * 64, area)
        observations = {"refusal": refusal}

        def path_missing():
            observations["path_present"] = os.path.lexists(root["path"])
            return not observations["path_present"]

        failed = first_failed_predicate([
            ("refusal_kind", lambda: refusal["kind"] != "usage"),
            ("path_present", path_missing),
        ])
        if failed is not None:
            raise Fault("R9", failure_detail(failed, failure_summary(observations)))
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
        failed, observations = root_kept_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R10", failure_detail(
                failed, run_report_summary(run_doc["data"], root["root_id"], observations)))
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
            raise Fault("R11", failure_detail(
                "pending_row_outcome", run_report_summary(run_doc["data"], root["root_id"])))
        pending = self.pending_intents()
        observations = {"pending_intents": len(pending), "expected_pending_intents": 1}

        def pending_identity_missing():
            observations["pending_intent_identity_present"] = pending[0].get("identity") is not None
            return not observations["pending_intent_identity_present"]

        failed = first_failed_predicate([
            ("pending_intent_count", lambda: observations["pending_intents"] != 1),
            ("pending_intent_identity", pending_identity_missing),
        ])
        if failed is not None:
            raise Fault("R11", failure_detail(failed, failure_summary(observations)))
        if not os.path.lexists(root["path"]):
            raise Fault("R11", "the unit moved before its rename succeeded")
        old_device = self.device
        detached = self.detach("intermediate")
        if not detached["closed"]:
            self.unknown_custody = True
            raise Infrastructure("detach", f"ordinary detach did not positively close the owned backing: {detached['reason']}")
        mounted = self.attach(first=False)
        remounted = self.device_after_remount != old_device
        self.remount = {"mount_before": old_mount, "mount_after": self.mount,
                        "backing": self.image, "volume_uuid": self.uuid,
                        "device_before": old_device, "device_after": self.device_after_remount,
                        "device_changed": remounted, "mount_relative_path": identity["mount_relative_path"],
                        "attachment": mounted, "closed_before_attach": detached["closed"]}
        _code, replay = self.real(plan_file, digest, expect=0)
        failed = replay_settlement_failure(replay["data"], self.pending_intents)
        if failed is not None:
            raise Fault("R11", failure_detail(
                failed, run_report_summary(replay["data"], root["root_id"])))
        root = dict(root, path=rebound_path(old_mount, self.mount, root["path"], identity, self.uuid))
        verify_rollback(root["path"], identity, identity_bytes, marker_bytes, payload, self.pending_intents())
        preview2, row2, plan2 = self.plan_and_run("R11-after", area, root, expect_eligible=True)
        failed = first_failed_predicate([
            ("mount_relative_path",
             lambda: row2["mount_relative_path"] != identity["mount_relative_path"]),
            ("identity_verdict", lambda: row2["identity"]["verdict"] != "verified"),
        ])
        if failed is not None:
            raise Fault("R11", failure_detail(failed, failure_summary({
                "mount_relative_path": row2["mount_relative_path"],
                "expected_mount_relative_path": identity["mount_relative_path"],
                "identity_verdict": row2["identity"]["verdict"]})))
        _code, released = self.real(plan2, preview2["data"]["plan_digest"], expect=0)
        failed, observations = released_after_replay_failure(
            released["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R11", failure_detail(
                failed, run_report_summary(released["data"], root["root_id"], observations)))
        return {"root": root["root_id"], "pending_before_detach": 1, "detach": detached,
                "device_before": old_device, "device_after": self.device_after_remount,
                "device_changed": remounted, "mount_before": old_mount, "mount_after": self.mount,
                "backing": self.image, "volume_uuid": self.uuid,
                "mount_relative_path": identity["mount_relative_path"], "remount": self.remount,
                "replay_pending": 0, "released_after": 1}

    def reset_registration(self):
        """Use a new private state and a supported registration, only after R11 passes."""
        if not self.results or self.results[-1]["case"] != "R11" or self.results[-1]["status"] != "passed":
            raise Fault("registration", "R11 has not completed successfully")
        remount = self.remount or {}
        attachment = remount.get("attachment") or {}
        failed = first_failed_predicate([
            ("remount_closed", lambda: remount.get("closed_before_attach") is not True),
            ("remount_attached", lambda: attachment.get("state") != "attached"),
            ("remount_backing", lambda: remount.get("backing") != self.image),
            ("remount_volume_uuid", lambda: remount.get("volume_uuid") != self.uuid),
            ("current_attachment", lambda: not self.attached),
            ("remount_mount", lambda: self.mount != remount.get("mount_after")),
        ])
        if failed is not None:
            raise Fault("registration", failure_detail(failed, failure_summary({
                "closed_before_attach": remount.get("closed_before_attach"),
                "attachment_state": attachment.get("state"),
                "backing": remount.get("backing"), "volume_uuid": remount.get("volume_uuid"),
                "mount_after": remount.get("mount_after"), "attached": self.attached,
                "mount": self.mount})))
        self.state = tempfile.mkdtemp(prefix="state-r12-", dir=self.root)
        self.env["BARGE_STATE_DIR"] = self.state
        _code, preview = self.barge(
            "volume", "register", "--mount", self.mount, "--availability", "permanent",
            "--durability", "expendable", "--root-dir", "", "--reserve", "1KiB",
            "--floor", "1KiB", "--declared-by", "release-proof", "--plan", expect=0)
        plan = preview["data"]["plan"]
        document = json.loads(plan)
        failed = first_failed_predicate([
            ("registration_volume_uuid",
             lambda: document["record"]["volume_uuid"] != self.uuid),
            ("registration_mount_hint",
             lambda: document["record"]["mount_hint"] != self.mount),
        ])
        if failed is not None:
            raise Fault("registration", failure_detail(failed, failure_summary({
                "volume_uuid": document["record"]["volume_uuid"], "expected_volume_uuid": self.uuid,
                "mount_hint": document["record"]["mount_hint"],
                "expected_mount_hint": self.mount})))
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
            raise Fault("R12", failure_detail(
                "released_count", run_report_summary(run_doc["data"], given["root_id"])))
        if not os.path.lexists(late["path"]):
            raise Fault("R12", "a root the presented plan did not carry was released")
        return {"given": given["root_id"], "not_given_kept": late["root_id"], "released": 1}

    def case_coordinated_registry_releases_unrelated_and_keeps_live_reference(self):
        area = self.fresh_area("c13", f"oldest-idle:{AREA_AGE_SECONDS}")
        protected = self.new_root(area, aged=True)
        unrelated = self.new_root(area, aged=True)
        checkout = os.path.join(self.home, "checkouts", "coordinated")
        self.coordinated_adoption(
            [self.oxide_row(protected, checkout, selected=True)],
            [self.settlement(unrelated["path"])],
        )
        preview, unrelated_row, plan_file = self.plan_and_run(
            "R13", area, unrelated, expect_eligible=True)
        kept = self.row_for(preview, protected)
        failed = first_failed_predicate([
            ("protected_eligible", lambda: kept["eligible"]),
            ("protected_blocked_by", lambda: kept["blocked_by"] != "references"),
            ("unrelated_eligible", lambda: not unrelated_row["eligible"]),
        ])
        if failed is not None:
            raise Fault("R13", row_failure(
                failed, kept,
                unrelated={"eligible": unrelated_row["eligible"],
                           "blocked_by": unrelated_row.get("blocked_by")}))
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        failed, observations = root_released_failure(
            run_doc["data"], unrelated["root_id"], lambda: os.path.lexists(unrelated["path"]))
        if failed is not None:
            raise Fault("R13", failure_detail(
                failed, run_report_summary(run_doc["data"], unrelated["root_id"], observations)))
        if not os.path.lexists(protected["path"]):
            raise Fault("R13", failure_detail(
                "protected_absent",
                run_report_summary(run_doc["data"], protected["root_id"],
                                   {"protected_present": False})))
        return {"protected": protected["root_id"], "unrelated": unrelated["root_id"],
                "released": 1, "settlement_named": unrelated["path"]}

    def case_malformed_coordinated_registry_refuses_the_batch(self):
        area = self.fresh_area("c14", f"oldest-idle:{AREA_AGE_SECONDS}")
        first = self.new_root(area, aged=True)
        second = self.new_root(area, aged=True)
        # The exact malformed shape: the completed paired controls beside a
        # coordinated image without its settlement set, as no /4-aware writer
        # ever publishes it. The fault is the image alone.
        self.peer_registration()
        self.write_pair((OXIDE_COORDINATED_FENCE, self.ready()),
                        (OXIDE_COORDINATED_FENCE, self.ready()))
        path = os.path.join(self.home, OXIDE_RELATIVE_REGISTRY)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        document = {
            "version": OXIDE_COORDINATED_REGISTRY_VERSION,
            "schema": 4,
            "revision": 2,
            "complete": True,
            "created_by": OXIDE_TRANSACTION,
            "rows": [],
        }
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(document, handle)
        self.current_area = area
        plan_file = self.plan_path("R14")
        # An unreadable coordinated set is a plan that needs a human: exit 3,
        # every row kept, and a presented run refuses without effects.
        _code, preview = self.preview(area, plan_file, expect=3)
        for root in (first, second):
            row = self.row_for(preview, root)
            if row["eligible"]:
                raise Fault("R14", row_failure("eligible", row, root=root["root_id"]))
        if preview["data"]["oxide"]["complete"]:
            raise Fault("R14", "a malformed coordinated registry reads as complete")
        refusal = self.refused(plan_file, preview["data"]["plan_digest"], area)
        if refusal["kind"] != "unknown_state":
            raise Fault("R14", f"the batch was refused as {refusal}, expected unknown_state")
        for root in (first, second):
            if not os.path.lexists(root["path"]):
                raise Fault("R14", failure_detail("path_present", failure_summary({
                    "root": root["root_id"], "path_present": False})))
        return {"roots_kept": 2, "refusal": refusal, "counterfactual_of": "R13",
                "flipped_condition": "settlement set present"}

    def case_coordinated_census_reports_the_registry(self):
        area = self.fresh_area("c15", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        checkout = os.path.join(self.home, "checkouts", "coordinated")
        settled = [self.settlement(os.path.join(self.mount, "gone"))]
        # The census resolves the registry from the internal pool list itself,
        # so the same coordinated document is published where the census reads
        # it: the default pool's own outputs folder.
        census_path = os.path.join(
            self.home, "Library", "Caches", "oxide", "build-pool", "outputs", "registry.json")
        self.coordinated_registry(
            [self.oxide_row(root, checkout, selected=True)], settled, path=census_path)
        _code, inspect_doc = self.barge("inspect", expect=0)
        census = inspect_doc["data"]["oxide_census"]
        document = census.get("document") or {}
        failed = first_failed_predicate([
            ("census_state", lambda: census.get("state") != "reported"),
            ("census_version",
             lambda: document.get("version") != OXIDE_COORDINATED_REGISTRY_VERSION),
            ("census_schema", lambda: document.get("schema") != 4),
            ("census_rows", lambda: (census.get("rows") or {}).get("count") != 1),
        ])
        if failed is not None:
            raise Fault("R15", failure_detail(failed, failure_summary({
                "state": census.get("state"), "document": document,
                "rows": (census.get("rows") or {}).get("count"),
                "path": census.get("path")})))
        # The malformed twin: the same format without its settlement set stays
        # unknown, never an empty registry.
        with open(census_path, "w", encoding="utf-8") as handle:
            json.dump({
                "version": OXIDE_COORDINATED_REGISTRY_VERSION,
                "schema": 4,
                "revision": 2,
                "complete": True,
                "created_by": "oxide",
                "rows": [],
            }, handle)
        _code, inspect_doc = self.barge("inspect", expect=0)
        census = inspect_doc["data"]["oxide_census"]
        detail = census.get("detail") or ""
        failed = first_failed_predicate([
            ("census_state", lambda: census.get("state") != "unknown_state"),
            ("census_reason", lambda: "settlement set" not in detail),
        ])
        if failed is not None:
            raise Fault("R15", failure_detail(failed, failure_summary({
                "state": census.get("state"),
                "detail": bounded_text(detail, FAILURE_TEXT_CHARS)})))
        return {"census_version": OXIDE_COORDINATED_REGISTRY_VERSION, "rows": 1,
                "malformed_state": "unknown_state"}

    def adoption_refusal(self, case, label, area, roots):
        """One incomplete coordinated adoption: the plan keeps and the run refuses.

        The whole batch is kept (never a single root), the plan exits its
        needs-human code, the presented run refuses as unknown state, and every
        named root is still on disk.
        """
        self.current_area = area
        plan_file = self.plan_path(f"{case}-{label}")
        _code, preview = self.preview(area, plan_file, expect=3)
        entries = preview["data"]
        if entries["oxide"]["complete"]:
            raise Fault(case, failure_detail(
                f"{label}_complete",
                failure_summary({"step": label, "oxide": entries["oxide"]})))
        for root in roots:
            row = self.row_for(preview, root)
            if row["eligible"]:
                raise Fault(case, failure_detail(
                    f"{label}_eligible",
                    failure_summary({"step": label, "root": root["root_id"],
                                     "blocked_by": row.get("blocked_by")})))
        refusal = self.refused(plan_file, entries["plan_digest"], area)
        if refusal["kind"] != "unknown_state":
            raise Fault(case, failure_detail(
                f"{label}_refusal",
                failure_summary({"step": label, "refusal": refusal,
                                 "expected_kind": "unknown_state"})))
        for root in roots:
            if not os.path.lexists(root["path"]):
                raise Fault(case, failure_detail(
                    f"{label}_released",
                    failure_summary({"step": label, "root": root["root_id"],
                                     "path_present": False})))
        return {"step": label, "roots_kept": len(roots), "refusal": refusal}

    def case_coordinated_adoption_group(self):
        """The coordinated adoption group: the emitted handoff releases, and every
        one-sided, mid-adoption or mixed generation keeps the whole batch.

        Positive first: the removable peer registered, both `/5` controls ready
        under one transaction, and the `/4` image that transaction created — over
        which an aged, unreferenced root releases. Then the four ways that
        handoff can be incomplete, each on a fresh area and its own eligible
        root: a mixed fence pair, an unequal transaction, a re-entered
        initialization, and a complete pair whose image is still the legacy `/2`
        one.
        """
        area = self.fresh_area("c16", f"oldest-idle:{AREA_AGE_SECONDS}")
        root = self.new_root(area, aged=True)
        self.coordinated_adoption(rows=[], settled=[])
        preview, unrelated, plan_file = self.plan_and_run(
            "R16", area, root, expect_eligible=True)
        _code, run_doc = self.real(plan_file, preview["data"]["plan_digest"], expect=0)
        failed, observations = root_released_failure(
            run_doc["data"], root["root_id"], lambda: os.path.lexists(root["path"]))
        if failed is not None:
            raise Fault("R16", failure_detail(failed, run_report_summary(
                run_doc["data"], root["root_id"], observations)))
        steps = [
            ("mixed-fence",
             [lambda: self.peer_registration(),
              lambda: self.write_pair(
                  (OXIDE_COORDINATED_FENCE, self.ready()),
                  (OXIDE_ROUTED_FENCE, self.ready())),
              lambda: self.coordinated_registry([], [])]),
            ("unequal-transaction",
             [lambda: self.peer_registration(),
              lambda: self.write_pair(
                  (OXIDE_COORDINATED_FENCE, self.ready()),
                  (OXIDE_COORDINATED_FENCE, self.ready("b" * 32))),
              lambda: self.coordinated_registry([], [])]),
            ("initializing",
             [lambda: self.peer_registration(),
              lambda: self.write_pair(
                  (OXIDE_COORDINATED_FENCE, self.ready()),
                  (OXIDE_COORDINATED_FENCE, self.initializing())),
              lambda: self.coordinated_registry([], [])]),
            ("one-sided-image",
             [lambda: self.peer_registration(),
              lambda: self.write_pair(
                  (OXIDE_COORDINATED_FENCE, self.ready()),
                  (OXIDE_COORDINATED_FENCE, self.ready())),
              lambda: self.legacy_registry([], complete=True)]),
        ]
        refusals = []
        for index, (label, writes) in enumerate(steps):
            step_area = self.fresh_area(f"c16-{index}", f"oldest-idle:{AREA_AGE_SECONDS}")
            kept = self.new_root(step_area, aged=True)
            for write in writes:
                write()
            refusals.append(self.adoption_refusal("R16", label, step_area, [kept]))
        return {"released": 1, "released_root": root["root_id"], "refusals": refusals}

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
            "route": self.route,
            "image": {"size_mib": IMAGE_MIB, "backing": self.image, "volume_uuid": self.uuid,
                      "attach_census": self.attach_census,
                      "device": self.device, "device_after_remount": self.device_after_remount,
                      "mounts": self.mount_history, "ownership": self.ownership,
                      "custody": self.custody, "remount": self.remount},
            "resources": {"limit_seconds": LIMIT_SECONDS, "scratch_budget_mib": SCRATCH_BUDGET_MIB,
                          "wall_seconds": round(time.time() - self.started, 3)},
            "closure": self.closure,
            "qualification_inputs": {
                "case_rows": f"every case row (R1-R{len(CASES)}) passed in this mode",
                "image_whole_backing_clearance": (self.closure or {}).get("closed"),
                "guard_receipt": {
                    "required": True,
                    "path": os.path.join(self.evidence or "<evidence>", "receipt.json"),
                    "state": "written by the qualified supervisor after this process exits; "
                             "the activator correlates its clean conclusion and its own detach/cleanup "
                             "record with this receipt before counting the pass",
                },
            },
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


def inside_owned(path, owned):
    """Whether a path is the owned root itself or strictly inside it, by resolution. Pure.

    The guard sets TMPDIR to the owned scratch root itself, so that equality must be
    accepted; a path that only shares a name prefix, or that resolves outside by `..`,
    must not be.
    """
    if not path or not owned:
        return False
    real_path, real_owned = os.path.realpath(path), os.path.realpath(owned)
    if real_owned == os.sep:
        return real_path.startswith(real_owned)
    return real_path == real_owned or real_path.startswith(real_owned + os.sep)


def guard_owned_root(scratch):
    """The guard's monitored owned root: the parent of the scratch directory it sets.

    The pinned supervisor creates one `oxide-proof.*` root directly under its
    `--root-parent` and then `scratch/` (and, by default, `evidence/`) inside it
    (`create_owned_root`, `owned_root` and `setup` in the pinned bytes), so the scratch
    directory's parent is that one monitored owned root.
    """
    if not isinstance(scratch, str) or not os.path.isabs(scratch):
        return None
    parent = os.path.dirname(os.path.normpath(scratch))
    return parent if parent not in ("", os.sep) else None


def guard_containment_facts(scratch_env=None, evidence_env=None):
    """What the host says about the guard's monitored layout. Read-only.

    One `lstat` and one real resolution per path; nothing is created. `scratch_env`
    and `evidence_env` default to the guard's own variables.
    """
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "") if scratch_env is None else scratch_env
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE", "") if evidence_env is None else evidence_env
    facts = {"scratch": scratch, "evidence": evidence,
             "root_parent": os.path.realpath(DESIGNATED_SCRATCH),
             "uid": os.getuid(), "scratch_facts": None, "evidence_facts": None,
             "root_facts": None}
    for key, path in (("scratch_facts", scratch), ("evidence_facts", evidence),
                      ("root_facts", guard_owned_root(scratch))):
        if not path:
            continue
        facts[key] = {"path": path, "realpath": None, "isdir": False, "islink": False,
                      "uid": None, "mode": None, "present": False}
        try:
            info = os.lstat(path)
        except OSError:
            continue
        facts[key].update(realpath=os.path.realpath(path), islink=os.path.islink(path),
                          isdir=os.path.isdir(path) and not os.path.islink(path),
                          uid=info.st_uid, mode=info.st_mode & 0o777, present=True)
    return facts


def _private_directory_problems(name, facts, uid, owner_only):
    """Why one guard-set directory is not this occurrence's private canonical directory."""
    problems = []
    if isinstance(facts, dict) and facts.get("islink") is True:
        return [f"the {name} {facts.get('path')!r} is a symlink"]
    if not isinstance(facts, dict) or facts.get("present") is not True or facts.get("isdir") is not True:
        return [f"the {name} {facts.get('path') if isinstance(facts, dict) else None!r} is not a directory"]
    path = facts.get("path")
    if not isinstance(path, str) or not os.path.isabs(path) or os.path.normpath(path) != path:
        problems.append(f"the {name} {path!r} is not an absolute canonical path")
    if facts.get("realpath") != path:
        problems.append(f"the {name} {path!r} resolves to {facts.get('realpath')!r}, so its path is an alias or a symlink")
    if facts.get("uid") != uid:
        problems.append(f"the {name} {path!r} is not owned by this account")
    mode = facts.get("mode")
    writable = 0o077 if owner_only else 0o022
    if not isinstance(mode, int) or mode & writable:
        problems.append(f"the {name} {path!r} mode {mode!r} is not private")
    return problems


def guard_containment_problems(facts):
    """Why the guard's scratch and evidence are not this occurrence's monitored layout.

    Pure. The pinned supervisor keeps them as siblings inside one owned root under the
    verified scratch parent: it creates `oxide-proof.*` directly under `--root-parent`
    (here the verified Stash scratch), then `scratch/` and, by default, `evidence/`
    inside that root. Evidence therefore need not be inside the scratch directory; it
    must be inside the same monitored owned root. This refuses an aliased or symlinked
    path, a path outside that owned root, a foreign or nested-off-parent root, a
    directory of another account, an unprivate directory, and evidence that aliases
    the scratch directory or the owned root itself.
    """
    problems = []
    uid = facts.get("uid")
    root_parent = facts.get("root_parent")
    scratch = facts.get("scratch_facts")
    problems += _private_directory_problems("scratch directory", scratch, uid, owner_only=False)
    problems += _private_directory_problems("evidence directory", facts.get("evidence_facts"), uid, owner_only=False)
    problems += _private_directory_problems("owned root", facts.get("root_facts"), uid, owner_only=True)
    scratch_path = scratch.get("path") if isinstance(scratch, dict) else None
    owned_root = guard_owned_root(scratch_path)
    if not owned_root:
        problems.append("the scratch directory does not name a guarded owned root")
    else:
        if os.path.dirname(owned_root) != root_parent:
            problems.append(f"the owned root {owned_root} is not directly under the verified scratch parent {root_parent}")
        evidence = facts.get("evidence_facts")
        evidence_path = evidence.get("path") if isinstance(evidence, dict) else None
        if not evidence_path:
            problems.append("the evidence directory is absent")
        else:
            if evidence_path == owned_root:
                problems.append("the evidence directory names the owned root itself")
            if os.path.normpath(evidence_path) == os.path.normpath(scratch_path):
                problems.append("the evidence directory aliases the scratch directory")
            if not inside_owned(evidence_path, owned_root) or os.path.realpath(evidence_path) == os.path.realpath(owned_root):
                problems.append(f"the evidence directory {evidence_path} is outside the monitored owned root {owned_root}")
    return problems


def guard_refusals(facts):
    """Why this process is not the pinned supervisor's guarded child. Pure.

    `facts` carries what the process observed: whether the evidence directory and the
    owned scratch root are directories, whether TMPDIR resolves inside the scratch, the
    observed containment problems of the guard's monitored layout, the installed
    supervisor's pin problems, and the carried scratch route. Nothing here reaches a
    build or a fixture.
    """
    refusals = []
    if facts.get("evidence_isdir") is not True:
        refusals.append("OXIDE_PROOF_EVIDENCE is not a directory; this is not the guard's run")
    if facts.get("scratch_isdir") is not True:
        refusals.append("OXIDE_TASK_SCRATCH_ROOT is unset or absent; this is not the guard's run")
    if facts.get("tmpdir_inside_scratch") is not True:
        refusals.append("TMPDIR is not inside the guard's owned scratch")
    refusals += list(facts.get("supervisor_problems") or [])
    refusals += route_refusals(facts.get("route"))
    if facts.get("deadline_ns") is None:
        refusals.append(f"{WALL_DEADLINE_ENV} does not name the guard's wall deadline")
    refusals += [f"guard containment: {problem}" for problem in facts.get("containment_problems") or []]
    return refusals


def guarded_run(mode, receipt_path, route, deadline_ns, parent, owned, stage=None,
                 verify=None, head=None, prove=None):
    """The guarded body: stage the source-bound build, verify it, then prove.

    Every step is a seam, so the self-check can drive the real order without a build or
    a fixture. A staging refusal returns 125 with no fixture effect at all: staging is
    the first thing that happens inside the guard.
    """
    stage = stage or stage_proof_binary
    verify = verify or staged_build_problems
    prove = prove or run_proof
    head = head if head is not None else git_head()
    try:
        manifest, stage_record = stage(parent, owned, deadline_ns=deadline_ns)
    except Infrastructure as error:
        print(json.dumps({"schema": SCHEMA, "ok": False, "stage": None,
                          "refused": f"staging the proof build failed: {error.detail}"}))
        return 125
    problems = verify(manifest["path"], head, parent)
    if problems:
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "; ".join(problems)}))
        return 125
    return prove(mode, manifest, stage_record, receipt_path, route)


def run_proof(mode, build, stage_record, receipt_path, route):
    """The premises, the cases, the closure, then the receipt."""
    binary = build["path"]
    evidence = os.path.dirname(os.path.abspath(receipt_path))
    proof = Proof(binary, receipt_path, mode == "diagnostic", build,
                  evidence=evidence, route=route)
    proof.build = dict(build, stage=stage_record)
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
                proof.closure = {"purpose": "final", "exit": None, "reaped": False,
                                 "mount_absent": False, "backing_attached": None,
                                 "closed": False, "custody": "retained", "mount": proof.mount,
                                 "detail": "unknown custody; retained without repair, retry or force detach"}
            elif proof.attached:
                proof.closure = proof.detach("final")
            else:
                proof.closure = {"purpose": "final", "exit": None, "reaped": False,
                                 "mount_absent": not os.path.lexists(proof.mount or ""),
                                 "backing_attached": None, "closed": False, "custody": "retained",
                                 "mount": proof.mount,
                                 "detail": "no attachment was established in this run"}
        except Exception as error:
            cancelled = f"the detach raised: {type(error).__name__}: {error}"
            proof.closure = {"purpose": "final", "exit": None, "reaped": False,
                             "mount_absent": False, "backing_attached": None, "closed": False,
                             "custody": "retained", "mount": proof.mount,
                             "detail": cancelled}

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
        and isinstance(proof.closure, dict)
        and proof.closure.get("closed") is True
        and proof.closure.get("backing_attached") is False
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


def inner_half(mode, receipt_path):
    """Refuse anything but the pinned supervisor's guarded child, then stage and prove."""
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE")
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "")
    tmpdir = os.environ.get("TMPDIR", "")
    containment = guard_containment_facts(scratch, evidence or "")
    facts = {
        "evidence_isdir": bool(evidence) and os.path.isdir(evidence),
        "scratch_isdir": bool(scratch) and os.path.isdir(scratch),
        "tmpdir_inside_scratch": inside_owned(tmpdir, scratch),
        "containment_problems": guard_containment_problems(containment),
        "supervisor_problems": installed_supervisor_problems(),
        "route": carried_route(),
        "deadline_ns": wall_deadline(),
    }
    refusals = guard_refusals(facts)
    if refusals:
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "; ".join(refusals)}))
        return 125
    if receipt_path is None:
        receipt_path = os.path.join(evidence, PROOF_RECEIPT)
    owned_root = guard_owned_root(scratch)
    return guarded_run(mode, receipt_path, facts["route"], facts["deadline_ns"], scratch,
                       {"owned root": owned_root, "evidence directory": evidence})


def carried_route():
    """The scratch route the outer half verified, or None when it carried none."""
    raw = os.environ.get(ROUTE_ENV)
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return None


def wall_deadline():
    """The absolute monotonic deadline the outer half launched the guard with, if any."""
    raw = os.environ.get(WALL_DEADLINE_ENV)
    try:
        value = int(raw)
    except (TypeError, ValueError):
        return None
    return value if value > 0 else None


def outer_half(mode):
    """Verify the pinned supervisor and this host's scratch route, then hand over to it.

    Nothing is staged and no fixture exists yet: entering the unchanged supervisor's
    bytes comes first, the scratch volume's UUID and shape second, and the guarded
    child stages its own source-bound build inside the run the guard created.
    """
    supervisor = supervisor_path()
    problems = installed_supervisor_problems()
    if problems:
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "; ".join(problems)}))
        return 125
    facts = scratch_route_facts()
    route_problems = scratch_route_problems(facts)
    if route_problems:
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "; ".join(route_problems), "route": route_document(facts)}))
        return 125
    env = dict(os.environ)
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env.pop("BARGE_PROOF_ROUTE", None)
    env[ROUTE_ENV] = json.dumps(route_document(facts), separators=(",", ":"))
    env[WALL_DEADLINE_ENV] = str(time.monotonic_ns() + (LIMIT_SECONDS - 2) * 1_000_000_000)
    env["BARGE_PROOF_SUPERVISOR"] = supervisor
    result = subprocess.run(supervisor_command(supervisor, mode), env=env)
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


def route_controls():
    """The scratch-route decisions, pinned on canned observations. Pure."""
    problems = []
    route = {"path": DESIGNATED_SCRATCH, "realpath": DESIGNATED_SCRATCH,
             "volume_mount": SCRATCH_VOLUME_MOUNT, "volume_uuid": SCRATCH_VOLUME_UUID,
             "owner_uid": 501, "mode": 0o700, "uid": 501, "present": True}
    if scratch_route_problems(route):
        problems.append("the pinned Stash scratch route is refused")
    for flipped in ({"volume_uuid": "00000000-0000-0000-0000-000000000000"},
                    {"volume_uuid": None},
                    {"path": SCRATCH_VOLUME_MOUNT},
                    {"path": os.path.expanduser("~/scratch/barge")},
                    {"realpath": "/private" + DESIGNATED_SCRATCH},
                    {"mode": 0o755},
                    {"owner_uid": 0},
                    {"present": False}):
        if not scratch_route_problems(dict(route, **flipped)):
            problems.append(f"a wrong scratch route {flipped} is accepted")
    document = route_document(route)
    if route_refusals(document):
        problems.append("the carried route document of the pinned scratch is refused")
    for flipped in ({"designated_scratch": SCRATCH_VOLUME_MOUNT},
                    {"volume_uuid": "0" * 8},
                    {"schema": "other/1"},
                    {"observed": dict(route, mode=0o777)}):
        if not route_refusals(dict(document, **flipped)):
            problems.append(f"a carried route document {flipped} is accepted")
    for absent in (None, {}, "not a document"):
        if not route_refusals(absent):
            problems.append("an absent scratch route document is accepted")
    return problems


def stage_controls():
    """Staging's location, environment and ordering decisions. Pure: reads source text."""
    problems = []
    guarded = {"owned root": "/guard/root", "evidence directory": "/guard/root/evidence"}
    if stage_parent_problems("/guard/root", guarded):
        problems.append("a stage directory inside the guarded root is refused")
    for parent in ("/elsewhere/stage", DESIGNATED_SCRATCH, os.sep, SCRATCH_VOLUME_MOUNT,
                   os.path.expanduser("~"), "relative/stage",
                   "/guard/root/../elsewhere"):
        if not stage_parent_problems(parent, guarded):
            problems.append(f"the stage parent {parent} is accepted")
    unguarded = {"designated scratch": DESIGNATED_SCRATCH}
    if not stage_parent_problems("/elsewhere/stage", unguarded):
        problems.append("the unguarded stage mode accepts a directory outside the designated scratch")
    if stage_parent_problems(os.path.join(DESIGNATED_SCRATCH, "proof-occurrence-x"), unguarded):
        problems.append("the stage mode refuses its own occurrence directory")
    if stage_directory({"schema": STAGE_SCHEMA, "features": [PROOF_FEATURE], "sha256": "0" * 64}):
        problems.append("a well-formed staged build is refused")
    for broken in ({"schema": "other/1"}, {"features": []}, {"sha256": None}):
        document = {"schema": STAGE_SCHEMA, "features": [PROOF_FEATURE], "sha256": "0" * 64}
        if not stage_directory(dict(document, **broken)):
            problems.append(f"a staged build {broken} is accepted")
    environment = stage_environment({"RUSTFLAGS": "-C target-cpu=native",
                                     "CARGO_ENCODED_RUSTFLAGS": "-C debug-assertions=on",
                                     "DYLD_INSERT_LIBRARIES": "/tmp/shim.dylib",
                                     "RUSTUP_TOOLCHAIN": "stable", "PATH": "/usr/bin"},
                                    "/guard/root")
    if stage_environment_problems(environment):
        problems.append("the guarded stage environment is refused after its own scrub")
    if environment.get("TMPDIR") != "/guard/root":
        problems.append("the stage environment does not place its temporary directory in the guard")
    for broken in ({"RUSTFLAGS": "-C debug-assertions=on"}, {"RUSTUP_TOOLCHAIN": "stable"},
                   {"RUSTUP_AUTO_INSTALL": "1"}, {"CARGO_NET_OFFLINE": "false"},
                   {"OXIDE_TASK_SCRATCH_ROOT": "/guard/root"}, {"TMPDIR": ""}):
        if not stage_environment_problems(dict(environment, **broken)):
            problems.append(f"a stage environment {broken} is accepted")
    with open(SCRIPT_PATH, encoding="utf-8") as handle:
        text = handle.read()
    outer = text.split("def outer_half(", 1)
    if len(outer) != 2 or "stage_proof_binary" in outer[1].split("\ndef ", 1)[0]:
        problems.append("the outer half stages the proof build before the supervisor is entered")
    inner = text.split("def inner_half(", 1)
    if len(inner) != 2:
        problems.append("the guarded half is missing")
    else:
        body = inner[1].split("\ndef ", 1)[0]
        if "guard_refusals(" not in body or "guarded_run(" not in body:
            problems.append("the guarded half does not refuse its premises before it stages")
        elif body.find("guard_refusals(") > body.find("guarded_run("):
            problems.append("the guarded half stages before it refuses a non-guard caller")
    return problems


def custody_controls():
    """The guard's image-custody indication and the attach decisions. Pure."""
    problems = []
    backing = "/guard/root/run/release-volume.sparseimage"
    if custody_document(backing, "/guard/root/run", "/guard/root/evidence") != {
            "version": CUSTODY_VERSION, "backings": [backing]}:
        problems.append("the image-custody indication changed the guard's own shape")
    if custody_document("/guard/root/evidence/proof.sparseimage", "/guard/root/run",
                        "/guard/root/evidence")["backings"] != ["/guard/root/evidence/proof.sparseimage"]:
        problems.append("a backing inside the evidence directory is refused")
    for path in ("/Users/tom/run/image.sparseimage", "/guard/root/../image.sparseimage",
                 "relative.sparseimage", "/guard/rootish/image.sparseimage"):
        try:
            custody_document(path, "/guard/root/run", "/guard/root/evidence")
        except ValueError:
            continue
        problems.append(f"a custody backing {path} outside the owned root is published")
    mount = "/Volumes/barge-release-proof-1-2-a"
    device = "/dev/disk9s1"
    # An APFS image attaches as a container entity plus its mounted volume entity. That
    # is the shape the guard's own census and the reviewed space pattern read, so this
    # positive control is the real one and not a single-row simplification.
    container = ("/dev/disk9", None)
    for rows in ([(device, mount)], [container, (device, mount)],
                 [container, (device, mount), ("/dev/disk9s2", None)]):
        document = attach_document(0, True, rows, mount, device)
        if document["state"] != "attached" or document.get("entities") != len(rows):
            problems.append(f"a positive attach is refused for the census {rows!r}: {document}")
    for exit_code, reaped, rows, reported in (
            (1, True, [(device, mount)], device),
            (0, False, [(device, mount)], device),
            (0, True, None, device),
            (0, True, [], device),
            (0, True, [container], device),
            (0, True, [container, (device, None)], device),
            (0, True, [container, (device, mount), ("/dev/disk9s2", "/Volumes/other")], device),
            (0, True, [container, (device, mount), (device, mount)], device),
            (0, True, [container, ("x", mount)], device),
            (0, True, [(device, mount)], None),
            (0, True, [(device, mount)], "")):
        if attach_document(exit_code, reaped, rows, mount, reported)["state"] == "attached":
            problems.append(f"an unknown attach {exit_code, reaped, rows, reported} is accepted")
    return problems


def closure_controls():
    """The detach authorization and the closure gate. Pure."""
    problems = []
    mount = "/Volumes/barge-release-proof-1-2-a"
    device = "/dev/disk9s1"
    uuid = "8A660440-C919-45C6-A3F1-3365B9BD2006"
    # The container-plus-volume shape is the reviewed positive binding, and single-entity
    # censuses stay acceptable so the same gate covers both real shapes.
    container = ("/dev/disk9", None)
    for rows in ([(device, mount)], [container, (device, mount)],
                 [(device, mount), ("/dev/disk9s2", None)]):
        problems_found = detach_binding_problems(rows, mount, device, uuid, uuid.lower())
        if problems_found:
            problems.append(f"a bound image is refused a detach for {rows!r}: {problems_found}")
    for rows, observed in (([], uuid), (None, uuid), ([container], uuid),
                           ([(device, mount), (device, mount)], uuid),
                           ([container, (device, mount), ("/dev/disk9s2", "/Volumes/other")], uuid),
                           ([container, ("/dev/disk9s2", mount)], uuid),
                           ([(device, mount)], "other"), ([(device, mount)], None)):
        if not detach_binding_problems(rows, mount, device, uuid, observed):
            problems.append(f"a detach is authorized by {rows, observed}")
    if not closure_document("final", 0, True, [], True)["closed"]:
        problems.append("a correct ordinary detach with whole-backing absence is not closed")
    for exit_code, reaped, rows, mount_absent in (
            (0, True, [(device, None)], True),
            (0, True, [container], True),
            (0, True, [container, (device, mount)], True),
            (1, True, [], True),
            (None, False, [], True),
            (0, True, None, True),
            (0, True, [], False)):
        document = closure_document("final", exit_code, reaped, rows, mount_absent)
        if document["closed"] or document["custody"] != "retained":
            problems.append(f"an unclosed detach {exit_code, reaped, rows, mount_absent} is accepted")
    if closure_document("intermediate", 0, True, [], True)["purpose"] != "intermediate":
        problems.append("the closure record does not keep the detach's purpose")
    return problems


def command_failure_controls():
    """The barge fault record, through the real command and counted-run composition. Pure.

    Every external command result is a labelled fake. The real `Proof.barge` runs on it,
    the real `run_cases` turns the raised fault into one failed row and an unexecuted
    remainder, and the real `receipt_document` publishes them; only the git, supervisor
    and binary-digest lookups are stubbed for the receipt. The controls assert the
    deciding counters, digests, root ids, outcomes and reasons survive a hostile leading
    member far beyond the old 300-character prefix, that omission facts are honest, that
    capture facts are stated for every stream, that a failure which parsed no document
    still publishes both captured prefixes, that an oversized capture states its own
    truncation, that an unusable result shape keeps the command record instead of raising
    into the generic exception branch, and that each such failure leaves the count
    unqualified with unknown custody retained and the remaining cases unexecuted.
    """
    problems = []
    module = sys.modules[__name__]
    binary = "/guard/root/proof-binary/barge"
    plan_digest = "d" * 64
    fresh_digest = "e" * 64

    def proof():
        instance = Proof.__new__(Proof)
        instance.binary = binary
        instance.env = {}
        instance.root = "/guard/root/run"
        instance.results = []
        instance.references = []
        instance.first_fault = None
        instance.unknown_custody = False
        instance.infrastructure = None
        instance.diagnostic = False
        instance.build = {"schema": "barge.release.proof-binary/1", "head": "a" * 40}
        instance.isolation = {"problems": []}
        instance.route = {}
        instance.image = "/guard/root/run/release-volume.sparseimage"
        instance.uuid = "8A660440-C919-45C6-A3F1-3365B9BD2006"
        instance.attach_census = None
        instance.device = "/dev/disk9s1"
        instance.device_after_remount = None
        instance.mount_history = []
        instance.ownership = None
        instance.custody = None
        instance.remount = None
        instance.closure = {"closed": False, "custody": "retained"}
        instance.evidence = "/guard/root/evidence"
        instance.preflight = {"problems": []}
        instance.started = time.time()
        return instance

    def completed(payload, returncode=0, stderr=b""):
        stdout = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        return subprocess.CompletedProcess(args=[binary], returncode=returncode,
                                           stdout=stdout, stderr=stderr)

    def prefix_survived(detail, name, raw):
        """Whether one stream's own prefix and its capture flag survived. Pure."""
        truncated = len(raw) > CENSUS_PREFIX_BYTES
        shown = raw[:FAILURE_TEXT_CHARS].decode("utf-8", "replace") or "<empty>"
        return (f"{name} (capture_truncated={str(truncated).lower()}" in detail
                and shown in detail)

    # One exit-3 result whose deciding counters, digests, root id and reason all sit
    # beyond a 4,000-character leading member.
    document = {
        "schema": "barge.prune.run/1",
        "command": "prune.run",
        "ok": True,
        "data": {
            "area": {"namespace": "example", "area": "c01"},
            "padding": "p" * 4000,
            "plan_digest": plan_digest,
            "fresh_plan_digest": fresh_digest,
            "released_count": 0,
            "kept_count": 0,
            "stopped": None,
            "replay_pending": 0,
            "rows": [{"root_id": "root-late", "outcome": "kept",
                      "reason": "the fresh gate keeps the root: custody is not safe"}],
        },
    }
    payload = json.dumps(document).encode()
    try:
        with mock.patch("subprocess.run", lambda *args, **kwargs: completed(payload, 3)):
            proof().barge("prune", "--plan", "/guard/root/R1-plan.json", expect=0)
    except Fault as error:
        detail = error.detail
        failed = first_failed_predicate([
            ("kind", lambda: error.check != "barge"),
            ("exit", lambda: '"observed_exit": 3' not in detail or '"expected_exit": 0' not in detail),
            ("reaped", lambda: '"reaped": true' not in detail),
            ("complete", lambda: '"incomplete": false' not in detail),
            ("bytes", lambda: f'"stdout_bytes": {len(payload)}' not in detail),
            ("hash", lambda: hashlib.sha256(payload).hexdigest() not in detail),
            ("schema", lambda: "barge.prune.run/1" not in detail),
            ("presented", lambda: plan_digest not in detail),
            ("fresh", lambda: fresh_digest not in detail),
            ("released", lambda: '"released_count": 0' not in detail),
            ("kept", lambda: '"kept_count": 0' not in detail),
            ("replay", lambda: '"replay_pending": 0' not in detail),
            ("root", lambda: "root-late" not in detail),
            ("outcome", lambda: '"outcome": "kept"' not in detail),
            ("reason", lambda: "the fresh gate keeps the root" not in detail),
            ("bound", lambda: len(detail) > BARGE_FAULT_DETAIL_CHARS),
            ("not_raw", lambda: "p" * 4000 in detail),
        ])
        if failed is not None:
            problems.append(f"the exit-3 fault record lost {failed}: {detail[:600]}")
    else:
        problems.append("an unexpected barge exit did not raise")

    # A result that is not JSON, does not decode, or never completes keeps the command's
    # own bytes and states the document as unobserved; malformed captures are never guessed
    # into fields, and each captured stream's own prefix survives even though no document
    # parsed.
    def timeout_runner():
        raise subprocess.TimeoutExpired(cmd=binary, timeout=120,
                                        output=b"partial stdout", stderr=b"partial stderr")

    def unrunnable_runner():
        raise OSError("the child cannot start")

    unparsed = (
        ("non-JSON", lambda: completed(b"not json at all", 0, b"the parse failed here"),
         b"not json at all", b"the parse failed here", False, "no usable JSON"),
        ("non-UTF-8", lambda: completed(b"\xff\xfe\x00", 0, b"the decode failed here"),
         b"\xff\xfe\x00", b"the decode failed here", False, "no usable JSON"),
        ("timeout", timeout_runner, b"partial stdout", b"partial stderr", True,
         "did not complete"),
        ("unrunnable", unrunnable_runner, b"", b"", True, "did not complete"),
    )
    for label, runner, stdout, stderr, incomplete, because in unparsed:
        try:
            with mock.patch("subprocess.run", lambda *args, **kwargs: runner()):
                proof().barge("prune", "--plan", "/guard/root/R1-plan.json", expect=0)
        except Fault as error:
            detail = error.detail
            failed = first_failed_predicate([
                ("kind", lambda: error.check != "barge"),
                ("because", lambda: because not in detail),
                ("bytes", lambda: f'"stdout_bytes": {len(stdout)}' not in detail
                 or f'"stderr_bytes": {len(stderr)}' not in detail),
                ("hash", lambda: hashlib.sha256(stdout).hexdigest() not in detail
                 or hashlib.sha256(stderr).hexdigest() not in detail),
                ("incomplete", lambda: f'"incomplete": {str(incomplete).lower()}' not in detail),
                ("no_deciding",
                 lambda: "deciding unobserved (no usable document)" not in detail),
                ("stdout_prefix", lambda: not prefix_survived(detail, "stdout_prefix", stdout)),
                ("stderr_prefix", lambda: not prefix_survived(detail, "stderr_prefix", stderr)),
            ])
            if failed is not None:
                problems.append(f"the {label} fault record lost {failed}: {detail[:600]}")
        else:
            problems.append(f"a {label} barge result did not raise")

    # A capture larger than the record's own prefix bound keeps its honest truncation facts
    # and the whole record stays inside its allowance.
    oversized = b"s" * (CENSUS_PREFIX_BYTES + 1000)
    try:
        with mock.patch("subprocess.run",
                        lambda *args, **kwargs: completed(oversized, 0, b"e" * 900)):
            proof().barge("prune", "--plan", "/guard/root/R1-plan.json", expect=0)
    except Fault as error:
        detail = error.detail
        failed = first_failed_predicate([
            ("kind", lambda: error.check != "barge"),
            ("stdout_capture", lambda: not prefix_survived(detail, "stdout_prefix", oversized)),
            ("stdout_shown",
             lambda: f"shown {FAILURE_TEXT_CHARS} of {CENSUS_PREFIX_BYTES} captured chars"
             not in detail),
            ("stderr_capture", lambda: not prefix_survived(detail, "stderr_prefix", b"e" * 900)),
            ("stderr_shown",
             lambda: f"shown {FAILURE_TEXT_CHARS} of 900 captured chars" not in detail),
            ("bytes", lambda: f'"stdout_bytes": {len(oversized)}' not in detail),
            ("bound", lambda: len(detail) > BARGE_FAULT_DETAIL_CHARS),
        ])
        if failed is not None:
            problems.append(f"an oversized capture lost {failed}: {detail[:600]}")
    else:
        problems.append("an oversized barge result did not raise")

    # The real counted composition: one fault becomes R1 failed, R2-R16 unexecuted, the
    # receipt unqualified, and unknown custody is retained for the stop/image decision.
    def composed(runner):
        instance = proof()

        def failing_case():
            return instance.barge("prune", "--plan", "/guard/root/R1-plan.json", expect=0)

        instance.case_released_one_synthetic_root = failing_case
        with mock.patch("subprocess.run", lambda *args, **kwargs: runner()), \
                mock.patch.object(module, "git_head", lambda: "a" * 40), \
                mock.patch.object(module, "sha256_file", lambda _path: "f" * 64), \
                mock.patch.object(module, "supervisor_path", lambda: "/guard/supervisor.py"):
            instance.run_cases()
            receipt = instance.receipt_document(cancelled=False)
        return instance, receipt

    def composition_problem(label, instance, receipt):
        """That one failed case stopped the count and retained unknown custody. Pure."""
        rows = receipt.get("cases") or []
        first = rows[0] if rows else {}
        unexecuted = [row.get("case") for row in rows if row.get("status") == "unexecuted"]
        failed = first_failed_predicate([
            ("qualified", lambda: receipt.get("qualified") is not False),
            ("mode", lambda: receipt.get("mode") != "counted"),
            ("first_fault", lambda: receipt.get("first_fault") != "R1"),
            ("all_cases", lambda: receipt.get("all_cases_passed") is not False),
            ("row_count", lambda: len(rows) != len(CASES)),
            ("first_status",
             lambda: first.get("status") != "failed" or first.get("kind") != "barge"),
            ("unexecuted", lambda: unexecuted != [case["id"] for case in CASES[1:]]),
            ("retained", lambda: instance.unknown_custody is not True),
        ])
        if failed is None:
            return first, None
        return first, f"the {label} composition lost {failed}: {json.dumps(receipt)[:600]}"

    instance, receipt = composed(lambda: completed(payload, 3))
    first, problem = composition_problem("exit-3", instance, receipt)
    if problem is not None:
        problems.append(problem)
    else:
        decided = first.get("detail") or ""
        failed = first_failed_predicate([
            ("deciding", lambda: not all(needle in decided for needle in (
                "root-late", plan_digest, '"released_count": 0'))),
            ("stdout_prefix", lambda: not prefix_survived(decided, "stdout_prefix", payload)),
        ])
        if failed is not None:
            problems.append(f"the exit-3 composition lost {failed}: {decided[:600]}")

    # A syntactically valid but unusable result shape keeps the command record: the real
    # summary states the unusable member, the fault stays the barge kind instead of falling
    # into the generic exception branch, and the count is still unqualified.
    malformed_payload = json.dumps({
        "schema": "barge.prune.run/1", "command": "prune.run", "ok": True,
        "data": {"rows": 1},
    }).encode()
    malformed_instance, malformed_receipt = composed(
        lambda: completed(malformed_payload, 3, b"rows were not a list"))
    first, problem = composition_problem("unusable-result", malformed_instance,
                                         malformed_receipt)
    if problem is not None:
        problems.append(problem)
    else:
        decided = first.get("detail") or ""
        failed = first_failed_predicate([
            ("command", lambda: not all(needle in decided for needle in (
                binary, '"observed_exit": 3',
                f'"stdout_bytes": {len(malformed_payload)}',
                hashlib.sha256(malformed_payload).hexdigest()))),
            ("rows_shape", lambda: '"rows_shape": "unusable (int)"' not in decided),
            ("rows_stated", lambda: '"rows_stated": "unobserved"' not in decided),
            ("stderr_prefix", lambda: not prefix_survived(decided, "stderr_prefix",
                                                         b"rows were not a list")),
            ("no_exception", lambda: first.get("trace") is not None or "TypeError" in decided),
        ])
        if failed is not None:
            problems.append(f"the unusable-result composition lost {failed}: {decided[:600]}")

    # A result that parsed no document still publishes both captured prefixes through the
    # real counted composition.
    unparsed_instance, unparsed_receipt = composed(
        lambda: completed(b"not json at all", 0, b"the parse failed here"))
    first, problem = composition_problem("unparsed", unparsed_instance, unparsed_receipt)
    if problem is not None:
        problems.append(problem)
    else:
        decided = first.get("detail") or ""
        failed = first_failed_predicate([
            ("stdout_prefix", lambda: not prefix_survived(decided, "stdout_prefix",
                                                         b"not json at all")),
            ("stderr_prefix", lambda: not prefix_survived(decided, "stderr_prefix",
                                                         b"the parse failed here")),
            ("no_deciding",
             lambda: "deciding unobserved (no usable document)" not in decided),
        ])
        if failed is not None:
            problems.append(f"the unparsed composition lost {failed}: {decided[:600]}")
    return problems


def census_controls():
    """The attachment census, its fail-closed parser and the real detach composition. Pure.

    Every external fact is a labelled fake: canned `hdiutil info` property lists and a
    faked command runner. The composition controls drive the actual `Proof.detach`, so the
    pre-census binding, the bounded command record and the post-census closure are the
    real code. The historically observed nonempty three-entity final census is preserved
    as a failure, and no unreadable, malformed or truncated structure is ever absence.
    """
    problems = []
    backing = "/guard/root/run/release-volume.sparseimage"
    mount = "/Volumes/barge-release-proof-1-2-a"
    device = "/dev/disk9s1"
    container = "/dev/disk9"
    volume_uuid = "8A660440-C919-45C6-A3F1-3365B9BD2006"

    def plist_safe(value):
        """A plist-encodable copy: hdiutil omits absent members rather than writing null."""
        if isinstance(value, dict):
            return {key: plist_safe(item) for key, item in value.items() if item is not None}
        if isinstance(value, list):
            return [plist_safe(item) for item in value]
        return value

    def completed(payload, returncode=0, stderr=b""):
        stdout = payload if isinstance(payload, bytes) else plistlib.dumps(plist_safe(payload))
        return subprocess.CompletedProcess(args=["hdiutil"], returncode=returncode,
                                           stdout=stdout, stderr=stderr)

    def census(payload, backing_path=backing, returncode=0, **kwargs):
        runner = kwargs.pop("runner", None)
        stderr = kwargs.pop("stderr", b"")
        if runner is None:
            runner = lambda _argv: completed(payload, returncode, stderr)
        return attachment_census(backing_path, {}, runner=runner)

    def attached(entities, image_path=backing):
        return {"images": [{"image-path": image_path, "system-entities": entities}]}

    volume = {"dev-entry": device, "mount-point": mount, "volume-uuid": volume_uuid}
    container_entity = {"dev-entry": container}

    # Positives: a readable census that names no attachment of this backing is the only
    # whole-backing absence, and a readable attached census keeps its entity identities.
    for payload in ({"images": []},
                    {"images": [{"image-path": "/elsewhere/other.sparseimage",
                                 "system-entities": [{"dev-entry": "/dev/disk7s1"}]}]}):
        document = census(payload)
        if document["unreadable"] or document["rows"] != [] or document["matched"]:
            problems.append(f"a positive absence census reads as {document['rows']}: {document}")
        if document["stdout_bytes"] <= 0 or not document["stdout_sha256"]:
            problems.append("a census record keeps no captured byte count or digest")
    document = census(attached([container_entity, volume]))
    if document["unreadable"] or document["rows"] != [(container, None), (device, mount)]:
        problems.append(f"an attached census does not keep its entities: {document}")
    if document["entities"][1]["volume_uuid"] != volume_uuid:
        problems.append("an attached census loses a parsed volume UUID")
    document = census(attached([volume]), stderr=b"census warning\n")
    if (document["unreadable"] or document["incomplete"]
            or document["stderr_prefix"] != "census warning\n"
            or document["stderr_bytes"] != len(b"census warning\n")
            or document["stderr_sha256"] != hashlib.sha256(b"census warning\n").hexdigest()
            or document["command"]["stderr_bytes"] != len(b"census warning\n")):
        problems.append(f"a successful census drops its stderr evidence: {document}")

    # Every incomplete or malformed relevant structure is unreadable, never absence.
    malformed = [
        ("not a plist", b"<not a plist", 0),
        ("a plist that is not a dictionary", plistlib.dumps(["images"]), 0),
        ("no image list", {"version": 1}, 0),
        ("an image list of the wrong type", {"images": {}}, 0),
        ("an image entry that is not a dictionary", {"images": ["x"]}, 0),
        ("no image-path", {"images": [{"system-entities": [volume]}]}, 0),
        ("an empty image-path", {"images": [{"image-path": "", "system-entities": [volume]}]}, 0),
        ("no entity list for this backing",
         {"images": [{"image-path": backing}]}, 0),
        ("an entity list stated as text",
         {"images": [{"image-path": backing, "system-entities": "none"}]}, 0),
        ("an empty entity list for this backing",
         {"images": [{"image-path": backing, "system-entities": []}]}, 0),
        ("an entity list of the wrong type",
         {"images": [{"image-path": backing, "system-entities": {"dev-entry": device}}]}, 0),
        ("an entity that is not a dictionary",
         {"images": [{"image-path": backing, "system-entities": ["x"]}]}, 0),
        ("an entity with no device",
         {"images": [{"image-path": backing, "system-entities": [container_entity, {"mount-point": mount}]}]}, 0),
        ("an entity with an empty device",
         {"images": [{"image-path": backing, "system-entities": [{"dev-entry": ""}]}]}, 0),
        ("an entity with an empty mount point",
         {"images": [{"image-path": backing, "system-entities": [dict(volume, **{"mount-point": ""})]}]}, 0),
        ("an entity with a malformed volume UUID",
         {"images": [{"image-path": backing, "system-entities": [dict(volume, **{"volume-uuid": "not-a-uuid"})]}]}, 0),
        ("two images naming this backing",
         {"images": [{"image-path": backing, "system-entities": [volume]},
                     {"image-path": backing, "system-entities": [volume]}]}, 0),
        ("a non-zero census exit", attached([volume]), 1),
        ("a relative image-path",
         {"images": [{"image-path": "release-volume.sparseimage", "system-entities": [volume]}]}, 0),
        ("a dot-relative image-path",
         {"images": [{"image-path": "./release-volume.sparseimage", "system-entities": [volume]}]}, 0),
        ("a parent-relative image-path",
         {"images": [{"image-path": "/guard/root/../run/release-volume.sparseimage",
                      "system-entities": [volume]}]}, 0),
        ("a doubled-separator image-path",
         {"images": [{"image-path": "/guard//root/run/release-volume.sparseimage",
                      "system-entities": [volume]}]}, 0),
    ]
    for label, payload, returncode in malformed:
        document = census(payload, returncode=returncode)
        if not document["unreadable"] or document["rows"] is not None:
            problems.append(f"{label} reads as an answer: rows={document['rows']} problem={document['problem']}")
        if not document["problem"]:
            problems.append(f"{label} keeps no deciding reason")

    def raising(_argv):
        raise OSError("the census child cannot run")
    document = attachment_census(backing, {}, runner=raising)
    if (not document["unreadable"] or document["rows"] is not None or document["reaped"]
            or not document["incomplete"] or not document["problem"]):
        problems.append(f"a census that never ran reads as an answer: {document}")
    for label, backing_path in (("a relative backing", "release-volume.sparseimage"),
                                ("a dot-relative backing", "./release-volume.sparseimage"),
                                ("a parent-relative backing", "/guard/../release-volume.sparseimage")):
        document = census({"images": []}, backing_path=backing_path)
        if not document["unreadable"] or document["rows"] is not None:
            problems.append(f"{label} reads as an absence: {document['rows']}")

    def timing_out(_argv):
        raise subprocess.TimeoutExpired(cmd=["hdiutil", "info", "-plist"],
                                        timeout=INFO_TIMEOUT_SECONDS,
                                        output=b"partial census stdout",
                                        stderr=b"partial census stderr")
    document = attachment_census(backing, {}, runner=timing_out)
    failed = first_failed_predicate([
        ("unreadable", lambda: not document["unreadable"] or document["rows"] is not None),
        ("incomplete", lambda: document["incomplete"] is not True),
        ("reaped", lambda: document["reaped"] is not False),
        ("exit", lambda: document["exit"] is not None),
        ("problem", lambda: "bound" not in (document["problem"] or "")),
        ("stdout", lambda: document["stdout_prefix"] != "partial census stdout"),
        ("stdout_bytes", lambda: document["stdout_bytes"] != len(b"partial census stdout")),
        ("stdout_hash",
         lambda: document["stdout_sha256"] != hashlib.sha256(b"partial census stdout").hexdigest()),
        ("stderr", lambda: document["stderr_prefix"] != "partial census stderr"),
        ("stderr_bytes", lambda: document["stderr_bytes"] != len(b"partial census stderr")),
        ("stderr_hash",
         lambda: document["stderr_sha256"] != hashlib.sha256(b"partial census stderr").hexdigest()),
        ("command", lambda: document["command"]["incomplete"] is not True),
    ])
    if failed is not None:
        problems.append(f"a timed-out census loses its partial evidence at {failed}: {document}")

    # The raw prefix is bounded and states its truncation; the complete digest still binds
    # the whole captured document.
    big = plistlib.dumps({"images": [{"image-path": backing, "system-entities": [
        {"dev-entry": f"/dev/disk{index}s1"} for index in range(400)]}]})
    document = census(big)
    if not document["stdout_truncated"]:
        problems.append("a census larger than the prefix is not marked truncated")
    if len(document["stdout_prefix"].encode()) > CENSUS_PREFIX_BYTES:
        problems.append("the census raw prefix exceeds its bound")
    if document["stdout_bytes"] != len(big) or document["stdout_sha256"] != hashlib.sha256(big).hexdigest():
        problems.append("the census record does not bind the complete captured bytes")

    # The real detach composition: pre-census binding, the bounded command record and the
    # post-census closure, driven through `Proof.detach` with fake external facts only.
    def drive(pre_rows, post_rows, detach_exit=0, detach_error=None):
        proof = Proof.__new__(Proof)
        proof.attached = True
        proof.mount = mount
        proof.uuid = volume_uuid
        proof.device = device
        proof.image = backing
        proof.env = {}
        def census_for(rows):
            if rows is None:
                return census(b"<not a plist")
            if rows == []:
                return census({"images": []})
            return census(attached([{"dev-entry": row[0], "mount-point": row[1]}
                                    for row in rows]))
        censuses = iter([census_for(pre_rows), census_for(post_rows)])
        seen = []

        def detach_run(argv, **kwargs):
            seen.append(list(argv))
            if detach_error is not None:
                raise detach_error
            return subprocess.CompletedProcess(args=argv, returncode=detach_exit,
                                               stdout=b"", stderr=b"")
        module = sys.modules[__name__]
        with mock.patch.object(module, "attachment_census", lambda *args, **kwargs: next(censuses)), \
                mock.patch.object(module, "volume_uuid", lambda _mount: volume_uuid), \
                mock.patch("subprocess.run", detach_run), \
                mock.patch("os.path.lexists", lambda _path: False):
            document = proof.detach("final")
        return document, seen

    document, seen = drive([(container, None), (device, mount)], [])
    failed = first_failed_predicate([
        ("closed", lambda: not document["closed"]),
        ("command", lambda: document["command"]["argv"] != [HDIUTIL, "detach", mount]),
        ("exit", lambda: document["command"]["exit"] != 0),
        ("reaped", lambda: document["command"]["reaped"] is not True),
        ("hash", lambda: not document["command"]["stdout_sha256"]),
        ("pre_census", lambda: document["pre_census"]["rows"] != [(container, None), (device, mount)]),
        ("post_census", lambda: document["post_census"]["rows"] != []),
        ("ran_once", lambda: seen != [[HDIUTIL, "detach", mount]]),
    ])
    if failed is not None:
        problems.append(f"the real detach composition failed at {failed}: {document.get('reason')}")

    # The historically observed final census: three attached entities, none mounted. It
    # stays a retained, unqualified outcome, and the reason keeps the deciding count.
    document, _seen = drive([(container, None), (device, mount)],
                            [(container, None), (device, None), ("/dev/disk25s1", None)])
    if document["closed"] or document["custody"] != "retained":
        problems.append(f"the three-entity nonempty final census reads as closed: {document}")
    if "3 entities" not in (document.get("reason") or ""):
        problems.append(f"the three-entity failure does not keep its deciding count: {document.get('reason')}")

    # A detach killed at its bound keeps its partial captures and states its
    # incompleteness; it never closes the image.
    document, _seen = drive([(container, None), (device, mount)], [],
                            detach_error=subprocess.TimeoutExpired(
                                cmd=[HDIUTIL, "detach", mount],
                                timeout=DETACH_TIMEOUT_SECONDS,
                                output=b"partial detach stdout",
                                stderr=b"partial detach stderr"))
    failed = first_failed_predicate([
        ("closed", lambda: document["closed"]),
        ("custody", lambda: document["custody"] != "retained"),
        ("incomplete", lambda: document["command"]["incomplete"] is not True),
        ("reaped", lambda: document["command"]["reaped"] is not False),
        ("exit", lambda: document["command"]["exit"] is not None),
        ("stdout", lambda: document["command"]["stdout_prefix"] != "partial detach stdout"),
        ("stdout_hash",
         lambda: document["command"]["stdout_sha256"]
         != hashlib.sha256(b"partial detach stdout").hexdigest()),
        ("stderr", lambda: document["command"]["stderr_prefix"] != "partial detach stderr"),
        ("stderr_hash",
         lambda: document["command"]["stderr_sha256"]
         != hashlib.sha256(b"partial detach stderr").hexdigest()),
        ("reason", lambda: "did not complete" not in (document["reason"] or "")),
    ])
    if failed is not None:
        problems.append(f"a timed-out detach loses its partial evidence at {failed}: {document.get('reason')}")

    # An unreadable post-census is retained with the parser's own reason, and an unbound
    # pre-census refuses the detach before any command runs.
    document, seen = drive([(container, None), (device, mount)], None, detach_exit=0)
    if document["closed"] or "unreadable" not in (document.get("reason") or ""):
        problems.append(f"an unreadable post-census reads as an answer: {document.get('reason')}")
    proof = Proof.__new__(Proof)
    proof.attached = True
    proof.mount = mount
    proof.uuid = volume_uuid
    proof.device = device
    proof.image = backing
    proof.env = {}
    module = sys.modules[__name__]
    unbound_census = census({"images": []})
    with mock.patch.object(module, "attachment_census", lambda *args, **kwargs: unbound_census), \
            mock.patch.object(module, "volume_uuid", lambda _mount: volume_uuid), \
            mock.patch("subprocess.run", lambda *args, **kwargs: problems.append("a detach ran unbound")), \
            mock.patch("os.path.lexists", lambda _path: True):
        document = proof.detach("final")
    if document["custody"] != "unknown" or document["command"] is not None:
        problems.append(f"an unbound detach is not refused: {document}")
    if proof.attached is not True:
        problems.append("a refused detach forgets that the image is still attached")

    # The custody indication must be the guard's own evidence child, verified before attach.
    evidence = "/guard/root/evidence"
    if custody_target_problems(os.path.join(evidence, CUSTODY_NAME), evidence, evidence):
        problems.append("the guard's own custody path is refused")
    for path, observed_evidence, guard_evidence in (
            (os.path.join("/guard/root", CUSTODY_NAME), evidence, evidence),
            (os.path.join(evidence, "other.json"), evidence, evidence),
            (os.path.join(evidence, CUSTODY_NAME), "/guard/other", evidence),
            (os.path.join(evidence, CUSTODY_NAME), "relative/evidence", None)):
        if not custody_target_problems(path, observed_evidence, guard_evidence):
            problems.append(f"a custody target {path!r} in {observed_evidence!r} is accepted")
    return problems


def failure_report_controls():
    """Each failed oracle names its own predicate and keeps bounded decisive fields. Pure.

    Every case below uses canned documents and counting thunks only; none of it claims a
    live observation. The point is that a failure record can no longer absorb the deciding
    fields, and that the original predicate order and short-circuit are preserved.
    """
    problems = []
    if bounded_text("x" * 10, 4) != "xxxx...[+6 chars]":
        problems.append("bounded_text does not mark a capped value")
    if bounded_text(7) != 7 or bounded_text(None) is not None:
        problems.append("bounded_text does not pass non-strings through")

    # Order and short-circuit, with call counts: only checks up to the first failure run.
    calls = []

    def recorded(name, answer):
        def check():
            calls.append(name)
            return answer
        return check

    failed = first_failed_predicate([("a", recorded("a", False)), ("b", recorded("b", True)),
                                     ("c", recorded("c", True))])
    if failed != "b" or calls != ["a", "b"]:
        problems.append(f"first_failed_predicate order or short-circuit changed: {failed}, {calls}")
    if first_failed_predicate([("a", recorded("a", False)), ("b", recorded("b", False))]) is not None:
        problems.append("a conjunction that holds reports a failed predicate")

    # The R11 post-replay conjunction: each predicate discriminated on its own, plus the
    # actual passing conjunction, with the path observation counted.
    def document(count, rows, *, missing_rows=False):
        value = {"released_count": count, "kept_count": 0, "stopped": False, "replay_pending": 0}
        if not missing_rows:
            value["rows"] = rows
        return value

    def case(name, doc, expected, expected_calls, expected_observations):
        observed = []
        try:
            actual, observations = released_after_replay_failure(
                doc, "root-a", lambda: (observed.append(1), False)[1])
        except Exception as error:  # an eager conjunction reaches a member it must not
            problems.append(f"R11 post-replay predicate {name} raised {type(error).__name__}: {error}")
            return
        if actual != expected or len(observed) != expected_calls or observations != expected_observations:
            problems.append(
                f"R11 post-replay predicate {name}: got {actual!r}, {len(observed)} path reads, "
                f"{observations!r}")

    case("zero released", document(0, []), "released_count", 0, {})
    case("zero rows", document(1, []), "row_count", 1, {"path_present": False})
    case("multi-row", document(1, [{"root_id": "root-a"}, {"root_id": "root-b"}]),
         "row_count", 1, {"path_present": False})
    case("wrong root", document(1, [{"root_id": "root-b"}]),
         "root_id", 1, {"path_present": False})
    case("passing conjunction", document(1, [{"root_id": "root-a"}]),
         None, 1, {"path_present": False})
    # A path that is still present fails on its own name, and the filesystem is read once.
    observed = []
    try:
        actual, observations = released_after_replay_failure(
            document(1, [{"root_id": "root-a"}]), "root-a",
            lambda: (observed.append(1), True)[1])
        if actual != "path_present" or len(observed) != 1 or observations != {"path_present": True}:
            problems.append(f"R11 path predicate: got {actual!r}, {len(observed)} reads, {observations!r}")
    except Exception as error:
        problems.append(f"R11 path predicate raised {type(error).__name__}: {error}")
    # Short-circuit preserved: an unreached member is never demanded.
    try:
        actual, observations = released_after_replay_failure(
            document(0, [], missing_rows=True), "root-a", lambda: False)
        if actual != "released_count" or observations != {}:
            problems.append(f"an unreached rows member was demanded: {actual!r}, {observations!r}")
    except KeyError:
        problems.append("an unreached rows member was read")
    try:
        released_after_replay_failure(document(1, [], missing_rows=True), "root-a", lambda: False)
        problems.append("a reached rows member is not propagated when absent")
    except KeyError:
        pass

    # The "root must still be here" conjunction used by R2/R3/R5/R8/R10 (and R4's release):
    # the count decides first, the filesystem is read once, and the name discriminates.
    kept_document = {"released_count": 0, "kept_count": 1, "stopped": False,
                     "replay_pending": 0, "rows": [{"root_id": "root-a", "outcome": "kept"}]}
    observed = []
    failed, observations = root_kept_failure(
        kept_document, "root-a", lambda: (observed.append(1), True)[1])
    if failed is not None or len(observed) != 1 or observations != {"path_present": True}:
        problems.append(f"the kept-root conjunction rejects a kept root: {failed!r}, {observations!r}")
    observed = []
    failed, observations = root_kept_failure(
        {"released_count": 1}, "root-a", lambda: (observed.append(1), True)[1])
    if failed != "released_count" or observed or observations != {}:
        problems.append(f"the kept-root conjunction does not name a release: {failed!r}, {observations!r}")
    observed = []
    failed, observations = root_kept_failure(
        kept_document, "root-a", lambda: (observed.append(1), False)[1])
    if failed != "path_present" or len(observed) != 1 or observations != {"path_present": False}:
        problems.append(f"the kept-root conjunction does not name a missing path: {failed!r}, "
                        f"{observations!r}")
    # The release polarity is its own helper: a correct release (count 1, path gone) must
    # pass, and a root that stays on disk must name the path. Keep polarity is the mirror.
    observed = []
    failed, observations = root_released_failure(
        {"released_count": 1}, "root-a", lambda: (observed.append(1), False)[1])
    if failed is not None or len(observed) != 1 or observations != {"path_present": False}:
        problems.append(f"a correct release is refused: {failed!r}, {observations!r}")
    observed = []
    failed, observations = root_released_failure(
        {"released_count": 1}, "root-a", lambda: (observed.append(1), True)[1])
    if failed != "path_present" or len(observed) != 1 or observations != {"path_present": True}:
        problems.append(f"a root still on disk is not named: {failed!r}, {observations!r}")
    observed = []
    failed, observations = root_released_failure(
        {"released_count": 0}, "root-a", lambda: (observed.append(1), False)[1])
    if failed != "released_count" or observed or observations != {}:
        problems.append(f"a counted release is not named: {failed!r}, {observations!r}")
    # The two polarities stay opposites on the same document.
    document_for_polarity = {"released_count": 1}
    if root_kept_failure(document_for_polarity, "root-a", lambda: False)[0] != "released_count":
        problems.append("keep polarity accepts a release")
    if root_released_failure(document_for_polarity, "root-a", lambda: True)[0] != "path_present":
        problems.append("release polarity accepts a root that stays on disk")

    # The replay conjunction: the intent list is only read when the document held.
    calls = []
    if replay_settlement_failure({"replay_pending": 1}, lambda: calls.append(1) or [{"id": 1}]) != "replay_pending" or calls:
        problems.append("the replay conjunction read intents before the document's own count")
    calls = []
    if replay_settlement_failure({"replay_pending": 0}, lambda: calls.append(1) or [{"id": 1}]) != "pending_intents" or len(calls) != 1:
        problems.append("the replay conjunction does not name a pending intent")
    if replay_settlement_failure({"replay_pending": 0}, lambda: []) is not None:
        problems.append("a settled replay reports a failure")
    for index, name in enumerate(("identity_record", "marker", "content", "inode")):
        flags = [True, True, True, True]
        flags[index] = False
        if rollback_failure(*flags) != name:
            problems.append(f"rollback does not discriminate {name}")
    if rollback_failure(True, True, True, True) is not None:
        problems.append("an intact rollback reports a changed observation")

    # The bounded record: forty rows with long reasons stay inside the allowance, the
    # decisive fields survive, and an unobserved path is never guessed.
    rows = [{"root_id": f"root-{index}", "outcome": "kept", "reason": "r" * 600}
            for index in range(40)]
    document_with_rows = {"released_count": 0, "kept_count": 40, "stopped": False,
                          "replay_pending": 0, "rows": rows, "area": "example/c11"}
    summary = run_report_summary(document_with_rows, "root-a")
    if len(json.dumps(summary, sort_keys=True)) > ROW_EVIDENCE_CHARS:
        problems.append("the run failure record exceeds the reporting allowance")
    for key, value in (("released_count", 0), ("kept_count", 40),
                       ("expected_root_id", "root-a"), ("path_present", "unobserved")):
        if summary.get(key) != value:
            problems.append(f"the run failure record drops {key}")
    returned = [row["root_id"] for row in rows]
    if (summary["row_root_ids"] != returned[:len(summary["row_root_ids"])]
            or len(summary["row_root_ids"]) + summary["row_root_ids_omitted"] != len(returned)):
        problems.append(f"the returned root ids are not a bounded prefix: "
                        f"{len(summary['row_root_ids'])}, {summary['row_root_ids_omitted']}")
    if (summary["rows_total"] != 40 or summary["rows_omitted"] == 0
            or len(summary["rows"]) + summary["rows_omitted"] != 40):
        problems.append(f"the run failure record does not bound its rows: {summary['rows_total']}, "
                        f"{summary['rows_omitted']}, {len(summary['rows'])}")
    # The returned root-id list needs its own bound: 200 rows with long ids must not blow
    # the allowance, and the omitted count must account for every id.
    hostile_rows = [{"root_id": "root-" + "x" * 200 + f"-{index}", "outcome": "kept",
                     "reason": "r" * 900} for index in range(200)]
    hostile = run_report_summary({"released_count": 0, "kept_count": 200, "stopped": False,
                                  "replay_pending": 0, "rows": hostile_rows}, "root-a")
    hostile_detail = failure_detail("released_count", hostile)
    if len(hostile_detail) > ROW_EVIDENCE_CHARS:
        problems.append(f"a 200-row failure record exceeds the allowance: {len(hostile_detail)}")
    if (len(hostile["row_root_ids"]) > ROW_ID_LIMIT
            or len(hostile["row_root_ids"]) + hostile["row_root_ids_omitted"] != 200):
        problems.append(f"the returned root ids are not bounded or accounted: "
                        f"{len(hostile['row_root_ids'])}, {hostile['row_root_ids_omitted']}")
    if (len(hostile["rows"]) + hostile["rows_omitted"] != 200
            or len(json.dumps(hostile, sort_keys=True)) > ROW_EVIDENCE_CHARS):
        problems.append("the 200-row record does not account for every row inside the allowance")
    small = run_report_summary({"released_count": 1, "kept_count": 0, "stopped": False,
                                "replay_pending": 0,
                                "rows": [{"root_id": "root-b", "outcome": "kept",
                                          "reason": "a reason"}]}, "root-a",
                               {"path_present": False})
    if small["rows"] != [{"root_id": "root-b", "outcome": "kept", "reason": "a reason"}]:
        problems.append(f"the per-row record changed: {small['rows']!r}")
    if small["path_present"] is not False:
        problems.append("a reached path observation is not reported")
    detail = failure_detail("root_id", small)
    if not detail.startswith("root_id failed: ") or '"expected_root_id": "root-a"' not in detail[:400]:
        problems.append(f"the failure detail does not lead with its predicate and fields: {detail[:120]}")
    # A field set alone is still inside the allowance.
    if len(json.dumps(failure_summary({f"field{index}": index for index in range(200)}),
                      sort_keys=True)) <= ROW_EVIDENCE_CHARS:
        problems.append("the failure-summary bound check did not run")
    return problems


def row_failure_controls():
    """Every plan-row oracle failure keeps the row's deciding facts. Pure.

    The reported defect was a failed native oracle whose record named the first
    blocker but not the deciding gate's own verdict and reason, so the occurrence
    could not be judged once its run root was gone. These controls pin the shared
    record, bound a hostile row, and drive the real R6 failure path — with the
    fixture's directory creation mocked — so the actual case body supplies the
    record rather than a copy of its predicate. No plan, image or child is touched.
    """
    problems = []
    row = {
        "absence": {"path": "/Volumes/x/checkouts/settled", "verdict": "present",
                    "reason": None},
        "blocked_by": "custody",
        "custody": "unknown",
        "custody_reason": ("custody is not safe: the open-descriptor scan is unreadable: "
                           "both reads of descriptor 16 of pid 52265 report it closed"),
        "eligible": False,
        "identity": {"verdict": "verified", "reason": None},
        "idle": {"last_activity_ms": 1700000000000, "reason": None},
        "reason": "custody is not safe",
        "references": [],
        "references_complete": True,
        "root_id": "r",
    }
    detail = row_failure("blocked_by", row)
    failed = first_failed_predicate([
        ("predicate", lambda: not detail.startswith("blocked_by failed: deciding ")),
        ("custody", lambda: '"custody": "unknown"' not in detail[:600]),
        ("custody_reason", lambda: "open-descriptor scan is unreadable" not in detail[:1600]),
        ("blocked_by", lambda: '"blocked_by": "custody"' not in detail[:600]),
        ("raw_row", lambda: '"root_id": "r"' not in detail),
    ])
    if failed is not None:
        problems.append(f"a plan-row failure record is missing {failed}: {detail[:300]}")
    # A long reason that sorts before the custody fields must not hide them: the
    # deciding fields are capped one by one, and the row id is one of them rather
    # than only a member of the truncated raw row.
    hostile = dict(row, padding="p" * 9000)
    hostile["absence"] = {"path": "/Volumes/x/" + "q" * 9000,
                          "reason": "r" * 9000, "verdict": "present"}
    hostile["root_id"] = "r"
    hostile_detail = row_failure("blocked_by", hostile)
    for name, needle in [
        ("omission marker", "...[+"),
        ("root_id", '"root_id": "r"'),
        ("custody verdict", '"custody": "unknown"'),
        ("custody reason", "open-descriptor scan is unreadable"),
        ("absence reason cap", '"absence_reason": "r'),
    ]:
        if needle not in hostile_detail:
            problems.append(
                f"a hostile plan-row failure record is missing its {name}: "
                f"{hostile_detail[:400]}")
    # The bound: every deciding field capped on its own, plus the bounded raw row.
    allowance = len(row_deciding_summary(row)) * ROW_FAILURE_FIELD_CHARS + \
        2 * ROW_EVIDENCE_CHARS
    if len(hostile_detail) > allowance:
        problems.append(
            f"a hostile plan-row failure record exceeds its allowance: "
            f"{len(hostile_detail)} > {allowance}")

    # The real R6 body, on canned seams: its own failure path must carry the
    # custody verdict and reason the plan returned.
    proof = Proof.__new__(Proof)
    proof.current_area = None
    proof.mount = "/Volumes/x"
    proof.fresh_area = lambda name, prune: "c06"
    proof.new_root = lambda area, aged=True: {"root_id": "r", "path": "/guard/root/c06/r"}
    proof.oxide_registry = lambda *args, **kwargs: None
    proof.oxide_row = lambda *args, **kwargs: {}
    proof.plan_and_run = lambda *args, **kwargs: (
        {"data": {"plan_digest": "d" * 64}}, row, "/guard/root/R6-present-plan.json")
    try:
        with mock.patch("os.makedirs"):
            proof.case_release_by_reference_needs_the_consumer_path_absent()
    except Fault as error:
        if error.check != "R6":
            problems.append(f"the real R6 failure path reported {error.check}")
        elif (not error.detail.startswith("blocked_by failed: deciding ")
              or '"custody": "unknown"' not in error.detail[:600]
              or "open-descriptor scan is unreadable" not in error.detail[:1600]):
            problems.append(
                f"the real R6 failure path dropped its deciding facts: {error.detail[:300]}")
    else:
        problems.append("the real R6 failure path did not fail on a custody-blocked row")
    return problems


def plan_controls():
    """A failing plan row stays judgeable from the receipt, and the oracle still decides.

    The row the CLI prints is a sorted-key document: `absence` sorts first and its own
    member set is larger than the driver's old 400-character cap, so a truncated detail
    could not name the gate. These controls pin the deciding fields ahead of the row and
    prove the eligibility requirement itself is unchanged.
    """
    problems = []
    absence = {"anchor": {"kind": "registered", "path": "/Volumes/x"},
               "ancestor": {"device": 16777248, "inode": 34,
                            "path": "/Volumes/x/checkouts",
                            "volume": {"kind": "registered", "volume_uuid": "u"}},
               "components_walked": 1, "leaf": {"name": "reappears", "result": "enoent"},
               "path": "/Volumes/x/checkouts/reappears" + "/" + "p" * 3000,
               "proved_ms": 1, "verdict": "absent"}
    references = [{"source": "barge-holds", "current": False},
                  {"source": "oxide-registry", "current": True},
                  {"source": "oxide-receipt", "current": False}]
    row = {"absence": absence, "blocked_by": "references", "custody": "safe",
           "eligible": False, "identity": {"verdict": "verified"},
           "idle": {"last_activity_ms": 1700000000000, "reason": None},
           "reason": "a reference still names this root: oxide-registry",
           "references": references, "references_complete": True, "root_id": "r"}
    summary = row_deciding_summary(row)
    expected = {"root_id": "r", "eligible": False, "blocked_by": "references",
                "reason": "a reference still names this root: oxide-registry",
                "identity_verdict": "verified", "identity_reason": None,
                "custody": "safe", "custody_reason": None,
                "last_activity_ms": 1700000000000, "idle_reason": None,
                "age_waived": None, "references_complete": True,
                "references_current": ["oxide-registry"],
                "absence_verdict": "absent", "absence_reason": None}
    if summary != expected:
        problems.append(f"the deciding summary changed: {json.dumps(summary, sort_keys=True)[:400]}")
    if row_deciding_summary({"eligible": True}) != dict(
            {key: None for key in expected} | {"eligible": True, "references_complete": None,
                                               "references_current": []}):
        problems.append("the deciding summary invents fields for a bare eligible row")
    # Ambiguity guard: the absence document alone is longer than a short cap, so the
    # deciding fields must lead the message rather than follow the row.
    proof = Proof.__new__(Proof)
    proof.root = "/guard/root"
    proof.current_area = None
    proof.preview = lambda *args, **kwargs: (0, {})
    proof.row_for = lambda document, root: row
    try:
        proof.plan_and_run("R10", "c10", {"root_id": "r"}, expect_eligible=True)
        problems.append("an ineligible row satisfies the eligible expectation")
    except Fault as error:
        detail = error.detail
        if not detail.startswith("expected eligible=True; deciding "):
            problems.append(f"the failure does not lead with the deciding fields: {detail[:120]}")
        if '"blocked_by": "references"' not in detail[:400]:
            problems.append("the first blocker is not inside the first 400 characters")
        # The row portion is capped at ROW_EVIDENCE_CHARS; the deciding summary that
        # precedes it is a fixed, small field set, so the whole record stays bounded.
        if len(detail) > ROW_EVIDENCE_CHARS + 1000:
            problems.append(f"the failure record is not bounded: {len(detail)} characters")
    # The same row must satisfy the inverse expectation, and an eligible row must pass
    # the eligible expectation: the decision rule itself is untouched.
    if proof.plan_and_run("R10", "c10", {"root_id": "r"}, expect_eligible=False)[1] is not row:
        problems.append("the inverse expectation does not return the same row")
    eligible = dict(row, eligible=True, blocked_by=None, reason=None)
    proof.row_for = lambda document, root: eligible
    if proof.plan_and_run("R10", "c10", {"root_id": "r"}, expect_eligible=True)[1] is not eligible:
        problems.append("an eligible row does not satisfy the eligible expectation")
    try:
        proof.plan_and_run("R10", "c10", {"root_id": "r"}, expect_eligible=False)
        problems.append("an eligible row satisfies the ineligible expectation")
    except Fault:
        pass
    return problems


def r13_case_controls():
    """Drive the real R13 body on fake external query seams. Pure.

    The control enters the actual case method, not a copied predicate: its plan
    subject is the unrelated root expected eligible, the real run presents the
    planned digest, the published settlement names the unrelated root's exact
    path, and both survival observations are actual filesystem queries the
    control fakes per scenario. The correct outcome (unrelated removed, its live
    referenced neighbour still present) passes; a missing protected root fails
    the survival requirement and a surviving unrelated root fails the release
    oracle. No CLI, filesystem, image or child is touched.
    """
    problems = []
    protected = {"root_id": "protected", "path": "/guard/root/c13/protected"}
    unrelated = {"root_id": "unrelated", "path": "/guard/root/c13/unrelated"}
    planned = ({"data": {"plan_digest": "d" * 64}},
               {"eligible": True, "blocked_by": None}, "/guard/root/R13-plan.json")
    kept = {"eligible": False, "blocked_by": "references"}
    run_report = {"released_count": 1, "kept_count": 0, "stopped": False,
                  "replay_pending": 0,
                  "rows": [{"root_id": "unrelated", "outcome": "released"}]}

    def lexists_for(protected_present, unrelated_present):
        def check(path):
            if path == protected["path"]:
                return protected_present
            if path == unrelated["path"]:
                return unrelated_present
            raise AssertionError(f"an unexpected path observation: {path}")
        return check

    def build():
        proof = Proof.__new__(Proof)
        proof.home = "/guard/home"
        calls = []
        roots = iter([protected, unrelated])
        proof.fresh_area = lambda *args, **kwargs: calls.append("fresh_area") or "c13"
        proof.new_root = lambda area, aged=True: calls.append(("new_root", area, aged)) or next(roots)
        proof.oxide_row = lambda root, checkout, **kwargs: \
            calls.append(("oxide_row", root["root_id"])) or {"row": root["root_id"]}
        proof.settlement = lambda path: {"root": path}
        proof.coordinated_adoption = lambda rows, settled, **kwargs: \
            calls.append(("coordinated_adoption", rows[0]["row"], settled[0]["root"]))
        proof.row_for = lambda document, root: calls.append(("row_for", root["root_id"])) or kept

        def planned_run(case, area, root, **kwargs):
            calls.append(("plan_and_run", case, root["root_id"], kwargs.get("expect_eligible")))
            return planned

        proof.plan_and_run = planned_run

        def real_run(plan_file, digest, expect=0):
            calls.append(("real", plan_file, digest, expect))
            return 0, {"data": run_report}

        proof.real = real_run
        return proof, calls

    # The correct outcome: the unrelated root is removed, the live referenced
    # neighbour survives, and every seam saw its real argument.
    proof, calls = build()
    try:
        with mock.patch("os.path.lexists", lexists_for(True, False)):
            evidence = proof.case_coordinated_registry_releases_unrelated_and_keeps_live_reference()
    except Fault as error:
        problems.append(f"the real R13 body refused its correct outcome: "
                        f"{error.check}: {error.detail[:FAILURE_TEXT_CHARS]}")
    else:
        failed = first_failed_predicate([
            ("plan_subject", lambda: ("plan_and_run", "R13", "unrelated", True) not in calls),
            ("row_for_protected", lambda: ("row_for", "protected") not in calls),
            ("run_digest",
             lambda: ("real", "/guard/root/R13-plan.json", "d" * 64, 0) not in calls),
            ("published_document",
             lambda: ("coordinated_adoption", "protected", unrelated["path"]) not in calls),
            ("released", lambda: evidence.get("released") != 1),
        ])
        if failed is not None:
            problems.append(f"the real R13 body failed its correct outcome at {failed}: "
                            f"{calls}, {evidence}")

    # A missing protected root fails the survival requirement, at the real method.
    proof, calls = build()
    try:
        with mock.patch("os.path.lexists", lexists_for(False, False)):
            proof.case_coordinated_registry_releases_unrelated_and_keeps_live_reference()
        problems.append("a missing protected root passed R13")
    except Fault as error:
        if error.check != "R13" or "protected_absent" not in error.detail:
            problems.append(f"an absent protected root failed as {error.check}: "
                            f"{error.detail[:FAILURE_TEXT_CHARS]}")

    # A surviving unrelated root fails the release oracle's own polarity.
    proof, calls = build()
    try:
        with mock.patch("os.path.lexists", lexists_for(True, True)):
            proof.case_coordinated_registry_releases_unrelated_and_keeps_live_reference()
        problems.append("a still-present unrelated root passed R13")
    except Fault as error:
        if error.check != "R13" or "path_present" not in error.detail:
            problems.append(f"a surviving unrelated root failed as {error.check}: "
                            f"{error.detail[:FAILURE_TEXT_CHARS]}")
    return problems


def r16_case_controls():
    """Drive the real R16 body and the real refusal step on scripted seams. Pure.

    The group's own body runs through its real fixture composition: the real
    `write_pair`, `pool_state`, `ready`, `initializing`, `coordinated_adoption`,
    `coordinated_registry` and `legacy_registry` execute, and only two external
    things are scripted — the fixture write primitive `publish` (which records
    the exact documents instead of writing them) and the CLI/filesystem queries.
    The real `adoption_refusal` is not substituted, so its whole-batch, exit-code,
    refusal-kind and survival composition runs four times inside the group.

    Controls, all through those real bodies: the captured documents must be the
    emitted handoff (both `/5` ready under one transaction, `/4` created by it)
    and the four incomplete variants; a surviving positive root, a complete plan
    at a refusal step, an eligible row there, a wrong refusal kind and a released
    root must each fail with the real fault.
    """
    problems = []
    released = {"root_id": "released", "path": "/guard/root/c16/released"}
    kept = {"root_id": "kept", "path": "/guard/root/c16/kept"}
    planned = ({"data": {"plan_digest": "d" * 64}},
               {"eligible": True, "blocked_by": None}, "/guard/root/R16-plan.json")
    run_report = {"released_count": 1, "kept_count": 0, "stopped": False,
                  "replay_pending": 0,
                  "rows": [{"root_id": "released", "outcome": "released"}]}

    def build(preview_complete=False, refusal_kind="unknown_state"):
        proof = Proof.__new__(Proof)
        proof.home = "/guard/home"
        proof.current_area = None
        published = []
        calls = []
        areas = iter(["c16", "c16-0", "c16-1", "c16-2", "c16-3"])

        def new_root(area, aged=True):
            calls.append(("new_root", area, aged))
            return released if area == "c16" else dict(kept, root_id=f"kept-{area}")

        proof.fresh_area = lambda name, prune: calls.append(("fresh_area", name)) or next(areas)
        proof.new_root = new_root
        proof.publish = lambda path, document: published.append((path, document))
        proof.plan_path = lambda case: f"/guard/root/{case}-plan.json"
        proof.plan_and_run = lambda case, area, root, **kwargs: calls.append(
            ("plan_and_run", case, root["root_id"], kwargs.get("expect_eligible"))) or planned
        proof.real = lambda plan_file, digest, expect=0: calls.append(
            ("real", plan_file, digest, expect)) or (0, {"data": run_report})
        proof.preview = lambda area, plan_file, named=(), expect=0: calls.append(
            ("preview", area, expect)) or (3, {"data": {
                "oxide": {"complete": preview_complete, "control": None},
                "plan_digest": "d" * 64}})
        proof.row_for = lambda document, root: calls.append(
            ("row_for", root["root_id"])) or {"eligible": False, "blocked_by": "references"}
        proof.refused = lambda plan_file, digest, area: calls.append(
            ("refused", area)) or {"exit": 4, "kind": refusal_kind}
        return proof, published, calls

    def lexists_for(released_present, kept_present=True):
        def check(path):
            if path == released["path"]:
                return released_present
            return kept_present
        return check

    def state_documents(published):
        return [document for path, document in published if path.endswith("state.json")]

    def registry_documents(published):
        return [document for path, document in published
                if path.endswith("registry.json")]

    def registration_documents(published):
        return [document for path, document in published
                if path.endswith(OXIDE_REGISTRATION_NAME)]

    # The exact paired footprint this fixture must publish under the private
    # HOME: the registration, both control states and the one registry image.
    # Nothing else may stand in for them.
    control = os.path.join("/guard/home", OXIDE_RELATIVE_CONTROL)
    # The producer's own registration field set, spelled here rather than read
    # back from the fixture's writer.
    expected_registration = {
        "version": 2,
        "mount_point": "/Volumes/Builds",
        "volume_uuid": OXIDE_POOL_VOLUME_UUID,
        "container_uuid": OXIDE_POOL_VOLUME_UUID,
        "reserve_bytes": 1,
        "cap_bytes": 2,
        "quota_bytes": None,
        "capacity_bytes": None,
        "registered_ms": 1,
        "mode": "ephemeral",
    }
    expected_positive_footprint = [
        os.path.join(control, OXIDE_REGISTRATION_NAME),
        os.path.join(control, "state.json"),
        os.path.join(os.path.dirname(control), "ledger.json"),
        os.path.join(control, "removable", "oxide-managed", "state.json"),
        os.path.join(control, "removable", "ledger.json"),
        os.path.join("/guard/home", OXIDE_RELATIVE_REGISTRY),
    ]

    def controls_are(published):
        states = state_documents(published)
        return [(document.get("owned_outputs"), document.get("outputs_init"))
                for document in states]

    proof, published, calls = build()
    try:
        with mock.patch("os.path.lexists", lexists_for(False)):
            evidence = proof.case_coordinated_adoption_group()
    except Fault as error:
        problems.append(f"the real R16 body refused its correct outcome: "
                        f"{error.check}: {error.detail[:FAILURE_TEXT_CHARS]}")
    else:
        # The positive step is the first pair the group publishes: both `/5`
        # controls ready under one transaction, and the `/4` image created by it.
        controls = controls_are(published)
        positive_pair = controls[:2]
        positive_image = registry_documents(published)[0]
        # The exact producer wire shapes this control insists on, spelled here
        # rather than read back from the fixture's own writers.
        ready_shape = {"ready": {"transaction": OXIDE_TRANSACTION, "ready_ms": 1}}
        unequal_shape = {"ready": {"transaction": "b" * 32, "ready_ms": 1}}
        initializing_shape = {"initializing": {"transaction": OXIDE_TRANSACTION,
                                               "started_ms": 1}}
        # Every incomplete variant is one of those exact shapes a producer can
        # leave behind mid-adoption.
        shapes = {(fence, json.dumps(init, sort_keys=True)) for fence, init in controls}
        required_shapes = {
            (OXIDE_COORDINATED_FENCE, json.dumps(ready_shape, sort_keys=True)),
            (OXIDE_ROUTED_FENCE, json.dumps(ready_shape, sort_keys=True)),
            (OXIDE_COORDINATED_FENCE, json.dumps(unequal_shape, sort_keys=True)),
            (OXIDE_COORDINATED_FENCE, json.dumps(initializing_shape, sort_keys=True)),
        }
        images = registry_documents(published)
        coordinated_images = [document for document in images
                              if document["version"] == OXIDE_COORDINATED_REGISTRY_VERSION]
        legacy_images = [document for document in images
                         if document["version"] == OXIDE_REGISTRY_VERSION]
        registrations = registration_documents(published)
        positive_paths = [path for path, _document in published][:6]
        failed = first_failed_predicate([
            ("positive_plan", lambda: ("plan_and_run", "R16", "released", True) not in calls),
            ("positive_run",
             lambda: ("real", "/guard/root/R16-plan.json", "d" * 64, 0) not in calls),
            ("peer_registration",
             lambda: not registrations or registrations[0] != expected_registration),
            ("positive_footprint",
             lambda: positive_paths != expected_positive_footprint),
            ("positive_pair",
             lambda: positive_pair != [(OXIDE_COORDINATED_FENCE, ready_shape)] * 2),
            ("positive_image",
             lambda: positive_image.get("version") != OXIDE_COORDINATED_REGISTRY_VERSION
             or positive_image.get("created_by") != OXIDE_TRANSACTION
             or positive_image.get("settled") != []),
            ("variants", lambda: not required_shapes <= shapes),
            ("coordinated_images",
             lambda: not coordinated_images or any(
                 document["created_by"] != OXIDE_TRANSACTION
                 or document.get("settled") != [] for document in coordinated_images)),
            ("one_sided_image", lambda: not legacy_images),
            ("refusal_steps",
             lambda: [call[1] for call in calls if call[0] == "preview"]
             != ["c16-0", "c16-1", "c16-2", "c16-3"]),
            ("refusals", lambda: [row.get("step") for row in evidence.get("refusals", [])]
             != ["mixed-fence", "unequal-transaction", "initializing", "one-sided-image"]),
            ("refusal_kind",
             lambda: any((row.get("refusal") or {}).get("kind") != "unknown_state"
                         for row in evidence.get("refusals", []))),
            ("released", lambda: evidence.get("released") != 1),
            ("root", lambda: evidence.get("released_root") != released["root_id"]),
        ])
        if failed is not None:
            problems.append(f"the real R16 body failed its correct outcome at {failed}: "
                            f"{calls}, {evidence}")

    # A surviving positive root fails the release oracle's own polarity.
    proof, _published, _calls = build()
    with mock.patch("os.path.lexists", lexists_for(True)):
        try:
            proof.case_coordinated_adoption_group()
            problems.append("a still-present positive root passed R16")
        except Fault as error:
            if error.check != "R16" or "path_present" not in error.detail:
                problems.append(f"a surviving positive root failed as {error.check}: "
                                f"{error.detail[:FAILURE_TEXT_CHARS]}")

    # A complete plan at a refusal step is refused by the real refusal body.
    proof, _published, _calls = build(preview_complete=True)
    with mock.patch("os.path.lexists", lexists_for(False)):
        try:
            proof.case_coordinated_adoption_group()
            problems.append("a complete coordinated adoption passed the refusal step")
        except Fault as error:
            if error.check != "R16" or "complete" not in error.detail:
                problems.append(f"a complete adoption failed the refusal step as {error.check}: "
                                f"{error.detail[:FAILURE_TEXT_CHARS]}")

    # A wrong refusal kind is refused by the real refusal body.
    proof, _published, _calls = build(refusal_kind="refused")
    with mock.patch("os.path.lexists", lexists_for(False)):
        try:
            proof.case_coordinated_adoption_group()
            problems.append("a non-unknown refusal kind passed the refusal step")
        except Fault as error:
            if error.check != "R16" or "expected_kind" not in error.detail:
                problems.append(f"a wrong refusal kind failed as {error.check}: "
                                f"{error.detail[:FAILURE_TEXT_CHARS]}")

    # A root released by a refused run fails the real survival requirement.
    proof, _published, _calls = build()
    with mock.patch("os.path.lexists", lexists_for(False, kept_present=False)):
        try:
            proof.case_coordinated_adoption_group()
            problems.append("a released root passed the refusal step")
        except Fault as error:
            if error.check != "R16" or "released" not in error.detail:
                problems.append(f"a released root failed as {error.check}: "
                                f"{error.detail[:FAILURE_TEXT_CHARS]}")

    # An eligible row at a refusal step is refused by the real refusal body, and
    # the emitted documents are the producer's own shapes.
    proof, _published, _calls = build()
    proof.row_for = lambda document, root: {"eligible": True, "blocked_by": None}
    with mock.patch("os.path.lexists", lexists_for(False)):
        try:
            proof.case_coordinated_adoption_group()
            problems.append("an eligible row passed the refusal step")
        except Fault as error:
            if error.check != "R16" or "eligible" not in error.detail:
                problems.append(f"an eligible row failed as {error.check}: "
                                f"{error.detail[:FAILURE_TEXT_CHARS]}")
    for name, document in [
        ("a pool control state", pool_state_document(OXIDE_COORDINATED_FENCE, None)),
        ("a pool ledger", ledger_document()),
        ("a coordinated image", coordinated_registry_document([], [], True, OXIDE_TRANSACTION)),
        ("a legacy image", legacy_registry_document([], True)),
    ]:
        if not isinstance(document, dict):
            problems.append(f"{name} is not an object")
    if "settled" in legacy_registry_document([], True):
        problems.append("a legacy image carries a settlement set")
    if pool_state_document(OXIDE_COORDINATED_FENCE, None).get("outputs_init") is not None:
        problems.append("an initialization-less control claims a record")
    return problems


def containment_facts(**overrides):
    """The pinned guard's real monitored layout as canned facts, plus overrides. Pure.

    The supervisor creates `oxide-proof.*` directly under `--root-parent`, then
    `scratch/` and, by default, `evidence/` inside it, so scratch and evidence are
    siblings inside one owned root: that is the positive shape these canned facts carry.
    """
    root = "/Volumes/Stash/scratch/barge/oxide-proof.abc123"
    facts = {
        "scratch": root + "/scratch",
        "evidence": root + "/evidence",
        "root_parent": os.path.realpath(DESIGNATED_SCRATCH),
        "uid": 501,
        "scratch_facts": {"path": root + "/scratch", "realpath": root + "/scratch",
                          "isdir": True, "uid": 501, "mode": 0o755, "present": True},
        "evidence_facts": {"path": root + "/evidence", "realpath": root + "/evidence",
                           "isdir": True, "uid": 501, "mode": 0o755, "present": True},
        "root_facts": {"path": root, "realpath": root, "isdir": True, "uid": 501,
                       "mode": 0o700, "present": True},
    }
    for key, value in overrides.items():
        if isinstance(value, dict) and isinstance(facts.get(key), dict):
            facts[key] = dict(facts[key], **value)
        else:
            facts[key] = value
    return facts


def containment_controls():
    """The guard's real scratch/evidence layout and its refusals. Pure."""
    problems = []
    root = "/Volumes/Stash/scratch/barge/oxide-proof.abc123"
    if guard_containment_problems(containment_facts()):
        problems.append("the pinned guard's sibling scratch and evidence layout is refused")
    for evidence in (root + "/scratch/supervisor-evidence.xyz",
                     root + "/scratch/evidence/nested"):
        facts = containment_facts(evidence=evidence,
                                  evidence_facts={"path": evidence, "realpath": evidence})
        if guard_containment_problems(facts):
            problems.append(f"a guard evidence directory inside the same owned root is refused: {evidence}")
    for name, facts in (
            ("evidence outside the owned root",
             containment_facts(evidence="/private/tmp/evidence",
                               evidence_facts={"path": "/private/tmp/evidence",
                                               "realpath": "/private/tmp/evidence"})),
            ("evidence naming the owned root", containment_facts(
                evidence=root, evidence_facts={"path": root, "realpath": root})),
            ("evidence aliasing the scratch directory", containment_facts(
                evidence=root + "/scratch",
                evidence_facts={"path": root + "/scratch", "realpath": root + "/scratch"})),
            ("an aliased evidence path",
             containment_facts(evidence=root + "/evidence-link",
                               evidence_facts={"path": root + "/evidence-link",
                                               "realpath": root + "/evidence"})),
            ("a symlinked evidence directory",
             containment_facts(evidence=root + "/evidence-link",
                               evidence_facts={"path": root + "/evidence-link", "islink": True})),
            ("a symlinked scratch directory",
             containment_facts(scratch_facts={"islink": True})),
            ("an aliased scratch path",
             containment_facts(scratch_facts={"realpath": "/private/tmp/elsewhere"})),
            ("a foreign owned root",
             containment_facts(scratch="/private/tmp/oxide-proof.abc123/scratch",
                               scratch_facts={"path": "/private/tmp/oxide-proof.abc123/scratch",
                                              "realpath": "/private/tmp/oxide-proof.abc123/scratch"},
                               evidence="/private/tmp/oxide-proof.abc123/evidence",
                               evidence_facts={"path": "/private/tmp/oxide-proof.abc123/evidence",
                                               "realpath": "/private/tmp/oxide-proof.abc123/evidence"},
                               root_facts={"path": "/private/tmp/oxide-proof.abc123",
                                           "realpath": "/private/tmp/oxide-proof.abc123"})),
            ("a root nested off the verified parent",
             containment_facts(
                 scratch=DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123/scratch",
                 scratch_facts={"path": DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123/scratch",
                                "realpath": DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123/scratch"},
                 evidence=DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123/evidence",
                 evidence_facts={"path": DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123/evidence",
                                 "realpath": DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123/evidence"},
                 root_facts={"path": DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123",
                             "realpath": DESIGNATED_SCRATCH + "/nested/oxide-proof.abc123"})),
            ("an unprivate owned root", containment_facts(root_facts={"mode": 0o755})),
            ("an owned root of another account", containment_facts(root_facts={"uid": 0})),
            ("a group-writable scratch", containment_facts(scratch_facts={"mode": 0o775})),
            ("a world-writable evidence directory", containment_facts(evidence_facts={"mode": 0o777})),
            ("an evidence directory of another account", containment_facts(evidence_facts={"uid": 0})),
            ("a scratch path that is not a directory", containment_facts(scratch_facts={"isdir": False})),
            ("an absent evidence directory", containment_facts(evidence_facts={"present": False})),
            ("a scratch path with no parent", containment_facts(scratch="scratch", scratch_facts=None)),
            ("a relative evidence path", containment_facts(evidence="evidence",
                                                           evidence_facts={"path": "evidence"}))):
        if not guard_containment_problems(facts):
            problems.append(f"the guard containment accepts {name}")
    # The real observation helper reports the pinned layout it is pointed at, and
    # nothing at all for an absent path.
    absent = guard_containment_facts("/nonexistent/scratch", "/nonexistent/evidence")
    if absent["scratch_facts"] is None or absent["evidence_facts"] is None:
        problems.append("the containment observation does not record the paths it was given")
    if not guard_containment_problems(absent):
        problems.append("an absent guard layout is accepted")
    return problems


def guarded_controls():
    """The guarded half's refusals and its stage-verify-prove order, on seams. Pure."""
    problems = []
    route = route_document({"path": DESIGNATED_SCRATCH, "realpath": DESIGNATED_SCRATCH,
                            "volume_mount": SCRATCH_VOLUME_MOUNT, "volume_uuid": SCRATCH_VOLUME_UUID,
                            "owner_uid": 501, "mode": 0o700, "uid": 501, "present": True})
    good = {"evidence_isdir": True, "scratch_isdir": True, "tmpdir_inside_scratch": True,
            "containment_problems": [], "supervisor_problems": [], "route": route,
            "deadline_ns": 1}
    if guard_refusals(good):
        problems.append("the guarded premises are refused")
    if not guard_refusals(dict(good, containment_problems=["the evidence directory is outside"])):
        problems.append("a containment problem does not refuse the guarded run")
    if not inside_owned("/guard/root", "/guard/root"):
        problems.append("the owned root itself is not inside the owned root")
    if not inside_owned("/guard/root/evidence", "/guard/root"):
        problems.append("a directory inside the owned root is not inside it")
    for path, owned in (("/guard/rootish", "/guard/root"), ("/elsewhere", "/guard/root"),
                        ("/guard/root/../elsewhere", "/guard/root"), ("", "/guard/root"),
                        ("/guard/root", "")):
        if inside_owned(path, owned):
            problems.append(f"{path!r} is accepted as inside {owned!r}")
    for flipped in ({"evidence_isdir": False}, {"scratch_isdir": False},
                    {"tmpdir_inside_scratch": False},
                    {"supervisor_problems": ["the supervisor is sha256 x, not the pinned y"]},
                    {"route": None}, {"route": dict(route, volume_uuid="0")},
                    {"deadline_ns": None}):
        if not guard_refusals(dict(good, **flipped)):
            problems.append(f"a guarded run accepts {flipped}")
    order = []
    staged = {"schema": STAGE_SCHEMA, "features": [PROOF_FEATURE], "sha256": "0" * 64,
              "head": "0" * 40, "path": "/guard/root/proof-binary-x/barge"}
    def fake_stage(parent, owned, deadline_ns=None):
        order.append("stage")
        return staged, {"seconds": 0.0}
    def fake_verify(binary, head, root):
        order.append("verify")
        return []
    def fake_prove(mode, build, record, receipt_path, guarded_route):
        order.append("prove")
        return 0
    def guarded(**overrides):
        arguments = {"stage": fake_stage, "verify": fake_verify, "head": "0" * 40,
                     "prove": fake_prove}
        arguments.update(overrides)
        with contextlib.redirect_stdout(io.StringIO()):
            return guarded_run("counted", "/guard/root/evidence/receipt.json", route, 1,
                               "/guard/root", {"owned root": "/guard/root"}, **arguments)
    if guarded() != 0 or order != ["stage", "verify", "prove"]:
        problems.append(f"the guarded order is not stage-verify-prove: {order}")
    order.clear()
    def failing_stage(parent, owned, deadline_ns=None):
        order.append("stage")
        raise Infrastructure("stage", "synthetic stage refusal")
    if guarded(stage=failing_stage) != 125 or order != ["stage"]:
        problems.append(f"a staging refusal reached a later effect: {order}")
    order.clear()
    def stale_verify(binary, head, root):
        order.append("verify")
        return ["the staged build is for another head"]
    if guarded(verify=stale_verify) != 125 or order != ["stage", "verify"]:
        problems.append(f"a stale staged build reached the proof: {order}")
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
    if not staged_build_problems(os.path.join(scratch, "absent", "barge"), "0" * 40, scratch):
        problems.append("a binary with no staged manifest is not refused")
    if not staged_build_problems("/elsewhere/barge", "0" * 40, scratch):
        problems.append("a binary outside the owned root is not refused")
    problems += route_controls()
    problems += guest_binding_controls()
    problems += stage_controls()
    problems += custody_controls()
    problems += closure_controls()
    problems += census_controls()
    problems += command_failure_controls()
    problems += containment_controls()
    problems += plan_controls()
    problems += row_failure_controls()
    problems += r13_case_controls()
    problems += r16_case_controls()
    problems += failure_report_controls()
    problems += guarded_controls()
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
    by_id = {case["id"]: case for case in CASES}
    setattr(fake, by_id["R11"]["method"], fail_remount)
    setattr(fake, by_id["R12"]["method"],
            lambda: problems.append("R12 ran after failed R11"))
    fake.run_cases()
    results = {result["case"]: result for result in fake.results}
    if results["R11"]["status"] != "failed":
        problems.append("the synthetic R11 fault did not fail R11")
    if results["R12"]["status"] != "skipped":
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
        "route": {
            "designated_scratch": DESIGNATED_SCRATCH,
            "volume_mount": SCRATCH_VOLUME_MOUNT,
            "volume_uuid": SCRATCH_VOLUME_UUID,
            "rule": "the mount's UUID, the canonical path, the account ownership and an "
                    "owner-only mode are verified before any staging or fixture effect; "
                    "a drive root, a home folder, a symlink or another volume refuses",
        },
        "build": {
            "feature": PROOF_FEATURE,
            "command": BUILD_COMMAND,
            "staged_under": "this occurrence's guarded root (a private exclusive directory "
                            "inside OXIDE_TASK_SCRATCH_ROOT), never a shared stage directory",
            "stage_budget": f"inside the guard's {LIMIT_SECONDS} s wall: staging and its waits "
                            "spend the supervisor's own bound and never a clock of their own",
            "stage_selector": "release:native:stage is a different, explicitly selected build; no "
                              "native occurrence in this task's sequence uses it, and a native run "
                              "never reads it",
            "production_build": "not run by this proof",
            "toolchain": TOOLCHAIN_CHANNEL,
        },
        "isolation": {
            "fixture_bindings": sorted(fixture_environment("<home>", "<state>", "<run>")),
            "oxide_directories": {"state": OXIDE_STATE_RELATIVE, "cache": OXIDE_CACHE_RELATIVE},
            "rule": "every bound fixture path resolves inside the supervisor's owned scratch; the reference control each case chose must too",
            "guard_containment": "the pinned supervisor keeps its scratch directory and its evidence directory as siblings inside one monitored owned root it creates directly under the verified Stash scratch parent; the driver requires both to be canonical, symlink-free, this account's and private, binds the owned root to the verified parent, and refuses an alias, an escape, a foreign or off-parent root and a mismatched occurrence (evidence is not required to be inside the scratch directory)",
        },
        "custody": {
            "indication": CUSTODY_NAME,
            "version": CUSTODY_VERSION,
            "published": "before the backing exists and before any attach, retained for the run",
            "intermediate_detach": "R11's ordinary detach, authorized by its own fresh rebind census",
            "final_detach": "the script's final ordinary detach, authorized by its own fresh rebind census; "
                            "the guard's cleanup is authorized separately by its own custody scan",
            "closure": "ordinary child success and reap plus whole-backing attachment absence plus an absent mount path",
        },
        "limitation": LIMITATION,
        "premise_dispositions": {
            "private_home_rustup": "every fixture child gets a private HOME/CARGO_HOME and the fixture state; the staged proof-feature compile is the ordinary managed Oxide build and therefore keeps the real account's HOME, CARGO_HOME and rustup home, writes nothing into the fixture, and is pinned to RUSTUP_TOOLCHAIN 1.97.1 with auto-install disabled and the committed lock; no fixture builds",
            "account_home": "fixture_environment binds HOME and BARGE_ACCOUNT_HOME; setup sets OXIDE_TEST_HOME_FROM_ENV for children",
            "environment": "setup scrubs inherited OXIDE, XDG and BARGE variables, then restores guard and fixture bindings; the stage environment drops both Rust flag variables",
            "adoption": "no checkout build in the fixture; roots are created explicitly by new_root",
            "cargo_argv": "no fake Cargo external subcommand is invoked",
            "owned_image": "one 64 MiB sparse image, attach uses owners on, UUID, device and whole-backing attachment verified, the guard's image-custody indication published before the backing exists, ordinary detach only, uncertain custody retained without repair, retry or force detach",
            "compiler_flags": "stage_environment removes RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS and DYLD_INSERT_LIBRARIES, drops the guard hooks so the build is an ordinary managed Oxide compilation, pins RUSTUP_TOOLCHAIN, RUSTUP_AUTO_INSTALL=0 and CARGO_NET_OFFLINE=true, and points TMPDIR at the guard",
            "temporary_storage": "the staged copy lives in one exclusive directory directly inside the guard's owned scratch root, before any fixture exists; setup then creates the private run root inside that same owned scratch, with the image backing inside the run root",
            "mktemp": "setup and reset_registration use tempfile.mkdtemp with an explicit owned parent; the stage directory is exclusive too",
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
            "counted": supervisor_command("<supervisor>", "counted"),
            "diagnostic": supervisor_command("<supervisor>", "diagnostic"),
        },
        "cases": [{"id": case["id"], "claim": case["claim"], "carried_by": case["carried_by"]}
                  for case in CASES],
        "activation_order": [
            "1. the owner activates one uncounted diagnostic at the exact head; it proves nothing alone. "
            "There is no standalone stage, prewarm or child smoke: the guard's own child stages the "
            "proof-feature build of HEAD inside the run it created, and `release:native:stage` is a "
            "different, explicitly selected build that this sequence never uses",
            "2. the owner activates one counted author pass at the same head",
            "3. a distinct reviewer activates its own counted pass at the same head",
            "4. the activator correlates the script receipt's case rows and whole-backing clearance "
            "with the supervisor's own receipt (its clean conclusion and its detach/cleanup record); "
            "a missing or unclear half is a missing proof",
        ],
    }


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic", "stage"])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--inner", choices=["counted", "diagnostic"])
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
              "the pinned supervisor, the scratch route, the guard's sibling containment, "
              "the stage route and the closure gate hold")
        return 0
    if args.plan:
        json.dump(plan_document(), sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0
    if args.inner:
        return inner_half(args.inner, args.receipt)
    if args.mode is None:
        parser.error("name a mode: stage, counted or diagnostic (or --self-check / --plan)")
    if args.mode == "stage":
        facts = scratch_route_facts()
        problems = scratch_route_problems(facts)
        if problems:
            print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "; ".join(problems),
                              "route": route_document(facts)}))
            return 125
        try:
            occurrence = tempfile.mkdtemp(prefix="proof-occurrence-", dir=DESIGNATED_SCRATCH)
            manifest, record = stage_proof_binary(
                occurrence, {"designated scratch": DESIGNATED_SCRATCH},
                deadline_ns=time.monotonic_ns() + STAGE_LIMIT_SECONDS * 1_000_000_000)
        except Infrastructure as error:
            print(json.dumps({"schema": SCHEMA, "ok": False,
                              "refused": f"staging failed: {error.detail}"}))
            return 125
        print(json.dumps({"schema": STAGE_SCHEMA, "ok": True, "route": route_document(facts),
                          "stage": record, **manifest}, indent=1))
        return 0
    return outer_half(args.mode)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
