#!/usr/bin/env python3
"""Barge's operation-journal native proof: the single-authority store's cases
on fresh private filesystem fixtures, under the qualified shared supervisor.

  python3 scripts/native/journal-proof.py --self-check  # apparatus only, effect-free
  python3 scripts/native/journal-proof.py --plan        # the plan, effect-free
  python3 scripts/native/journal-proof.py counted       # the author pass (owner-activated)
  python3 scripts/native/journal-proof.py diagnostic    # the uncounted twin (owner-activated)

It is owner-activated. No ordinary Effigy selector runs a counted or diagnostic
pass, and the declarations in `effigy.toml` activate nothing.

The composition is the established single-guard split, exactly like the
release and outputs proofs:

- **Outer (unguarded).** The genuine Effigy entry carries no guard-created
  variables — they do not exist until the supervisor creates them for its
  guarded child — so the outer reads none and supplies none. It verifies the
  pinned supervisor's bytes and the designated Stash route metadata, mints
  the wall deadline once, launches exactly one guarded child (the unchanged
  qualified `fcfda2b0…` supervisor around this script's `--inner` runner),
  parses exactly one inner receipt from it, and returns the actual guard
  exit. It creates no fixture, stage root or receipt.
- **Inner (guarded).** The inner validates that it really is the guard's run:
  the scratch and evidence directories the supervisor supplied are lstat'ed
  on their raw paths (a symlink is refused, never resolved), must be
  canonical directories owned by this account and free of group/other write
  bits — the guard creates them 0755, and the guard's owned root must be
  private 0700 — must sit inside the designated barge scratch on the
  verified private Stash volume, and the evidence must live inside the same
  guard-owned root; the wall deadline the outer minted must be present and
  well formed. It then creates its
  fixture and stage roots inside the owned scratch, stages the `proof-journal`
  feature build of HEAD through the managed Oxide build, mints the
  source-bound manifest and requires it to equal the pre-stage identity
  (drift refuses), re-hashes the staged binary before the first case, runs
  the twelve journal-only case groups, and writes the receipt into the
  guard's evidence directory before any cleanup.

One clock domain: `time.monotonic_ns()` in both halves. The outer mints
`BARGE_PROOF_WALL_DEADLINE_NS` exactly once before the launch (the release
handoff, with two seconds reserved); the inner validates it and never resets
it. The supervisor's own `OXIDE_PROOF_DEADLINE_NS` is the group deadline of
the guard's raw clock and is never subtracted from another clock. The
remaining wall time is measured freshly before the stage, before every case,
and at publication; every executor timeout is capped by the positive
remaining time minus the original 30 s closure margin; at or after the cutoff
nothing new is spawned — counted runs record skipped/unexecuted states
honestly, diagnostics record every case and stay `qualified: false`.

Neither mode ever reports `qualified: true` with an unverified supervisor or
route, a missing or changed source or manifest, a replaced staged binary,
duplicate, missing or unknown cases, an unconfirmed child custody, a fault,
or an expired budget. The inner native outcome is one fact; the guard's own
group closure is a separate fact the supervisor owns, and this script never
asserts it. The driver labels every injected failure in its verdict evidence.
No output root is deleted, no image is attached, and no real account, drive
or install is touched. The bounds — 300 s wall, 512 MiB owned scratch, no
image — remain unactivated here; the fit is unmeasured until the
owner-activated diagnostic.
"""

import argparse
import contextlib
import gc
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import types

sys.dont_write_bytecode = True

SCRIPT_PATH = os.path.abspath(__file__)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT_PATH)))


def _import_release_proof():
    """Load the established proof helpers from `release-proof.py` (hyphenated)."""
    path = os.path.join(os.path.dirname(SCRIPT_PATH), "release-proof.py")
    spec = importlib.util.spec_from_file_location("barge_release_proof", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


base = _import_release_proof()

SCHEMA = "barge.journal.native/1"
PLAN_SCHEMA = "barge.journal.native.plan/1"
RECEIPT_NAME = "journal-proof-receipt.json"
DRIVER_NAME = "journal_proof"
DRIVER_SOURCE = os.path.join(REPO_ROOT, "crates", "barge", "examples", f"{DRIVER_NAME}.rs")
STORE_SOURCE = os.path.join(REPO_ROOT, "crates", "barge", "src", "operation.rs")
MANIFEST = os.path.join(REPO_ROOT, "crates", "barge", "Cargo.toml")
CARGO_LOCK = os.path.join(REPO_ROOT, "Cargo.lock")
TOOLCHAIN_FILE = os.path.join(REPO_ROOT, "rust-toolchain.toml")
EFFIGY = os.path.join(REPO_ROOT, "effigy.toml")
PROOF_FEATURE = "proof-journal"
LIMIT_SECONDS = base.LIMIT_SECONDS
SCRATCH_BUDGET_MIB = base.SCRATCH_BUDGET_MIB
WORK_STOP_SECONDS = 30
STAGE_BASE_TIMEOUT = 240.0
CASE_BASE_TIMEOUT = 60.0
DESIGNATED_SCRATCH = base.DESIGNATED_SCRATCH
ROUTE_UUID = "8A660440-C919-45C6-A3F1-3365B9BD2006"
TOOLCHAIN_CHANNEL = base.TOOLCHAIN_CHANNEL
RUN_ROOT_PREFIX = "journal-proof-"
STAGE_COMMAND = [
    "oxide", "build", "-p", "barge", "--locked", "--offline",
    "--features", PROOF_FEATURE, "--example", DRIVER_NAME,
    "--message-format", "json",
]

CASES = [
    {"id": "J1", "method": "J1",
     "claim": "explicit initialization publishes the epoch then the binding "
              "with one creation id, a re-run finishes only a genuinely empty "
              "journal, and a destroyed or recreated pair refuses",
     "carried_by": [
         "explicit_initialization_publishes_one_pair_and_repeats_as_a_no_op",
         "initialization_finishes_only_a_genuinely_empty_epoch_only_journal",
         "a_binding_without_an_epoch_is_a_recreated_journal_never_re_initialized",
         "an_epoch_without_a_binding_is_incomplete_authority",
         "a_disagreeing_pair_is_unknown",
     ]},
    {"id": "J2", "method": "J2",
     "claim": "begin is idempotent per caller key with exact request, unit and "
              "hook matching; one owed request binds one operation; a unit a "
              "pending record binds is spoken for; a cancelled key is spent",
     "carried_by": [
         "the_same_request_answers_already_and_any_mismatch_refuses",
         "a_request_with_an_owed_finalize_is_bound_to_one_operation",
         "a_unit_a_pending_operation_binds_is_spoken_for",
         "a_barge_owned_begin_is_idempotent_on_unit_identity",
         "a_cancelled_unit_may_begin_again_but_a_pending_one_never",
     ]},
    {"id": "J3", "method": "J3",
     "claim": "at a valid full journal, the exact live request and the bound "
              "Barge-owned unit return their existing records, a mismatched "
              "request refuses, and a genuinely new allocation refuses "
              "without a write",
     "carried_by": [
         "idempotency_precedes_the_capacity_refusal_at_a_valid_full_journal",
         "the_row_bound_refuses_and_begin_refuses_an_exhausted_journal",
     ]},
    {"id": "J4", "method": "J4",
     "claim": "an injected record-publication failure leaves the journal "
              "unchanged and the allocation unresolved",
     "carried_by": [
         "a_crash_before_the_record_publish_leaves_the_journal_unchanged",
     ]},
    {"id": "J5", "method": "J5",
     "claim": "a stop between the record publication and the watermark leaves "
              "one verified allocation tail that the next begin reconciles "
              "before allocating",
     "carried_by": [
         "a_stop_between_the_record_and_the_watermark_is_one_recoverable_tail",
     ]},
    {"id": "J6", "method": "J6",
     "claim": "a sequence gap is proven loss and never silence, including a "
              "record lost after removal with the finalize owed",
     "carried_by": [
         "a_sequence_gap_is_proven_loss_and_never_silence",
         "a_record_lost_after_removal_with_the_finalize_owed_stays_unknown",
     ]},
    {"id": "J7", "method": "J7",
     "claim": "cancellation is a retained terminal, the ladder is "
              "adjacent-only, and the ack settles and freezes",
     "carried_by": [
         "cancellation_is_a_retained_pre_effect_transition",
         "the_ladder_advances_forward_only_and_terminals_are_frozen",
     ]},
    {"id": "J8", "method": "J8",
     "claim": "malformed, foreign-format, name-mismatched, stray and over-cap "
              "documents refuse, and the payload cap refuses at begin",
     "carried_by": [
         "malformed_unknown_format_and_name_mismatched_records_refuse",
         "a_stray_entry_or_leading_zero_name_refuses",
         "an_over_cap_document_refuses_the_read",
         "a_request_never_accepts_a_payload_over_its_cap_or_a_pre_set_capture_name",
     ]},
    {"id": "J9", "method": "J9",
     "claim": "a replaced or linked journal directory refuses with no "
              "substitute write, an unsafe mode or a foreign owner refuses "
              "every access, and a missing directory reads as unbound and "
              "refuses writes",
     "carried_by": [
         "a_replaced_journal_directory_is_never_followed",
         "an_unsafe_journal_directory_mode_refuses_every_access",
         "a_missing_journal_directory_reads_unbound_and_refuses_writes",
         "a_foreign_owned_journal_refuses_as_someone_elses_data",
         "a_foreign_owned_document_leaf_refuses_even_under_a_verified_parent",
         "an_undecided_current_account_refuses_every_access",
         "records_without_a_complete_authority_pair_refuse_as_unknown_not_resource_bound",
         "a_non_private_or_linked_state_document_is_never_followed",
     ]},
    {"id": "J10", "method": "J10",
     "claim": "the try-shared consult refuses immediately while the journal is "
              "locked and answers when free, and the shipped journal never "
              "mixes with the successor",
     "carried_by": [
         "the_consult_refuses_immediately_while_the_journal_is_locked",
         "the_consult_on_a_never_locked_journal_answers_unbound",
         "the_two_journals_coexist_and_never_mix",
     ]},
    {"id": "J11", "method": "J11",
     "claim": "capture and foreign-capture evidence publish before effects "
              "and stay bounded",
     "carried_by": [
         "capture_evidence_publishes_before_its_unlink_and_stays_bounded",
         "the_record_validator_refuses_a_capture_name_from_another_operation",
     ]},
    {"id": "J12", "method": "J12",
     "claim": "the read-only diagnostic reports findings without repairing",
     "carried_by": [
         "the_diagnostic_reports_findings_without_repairing",
     ]},
]

FORBIDDEN_TOKENS = ["-" + "force", "p" + "kill", "kill" + "all"]

PREMISES = (
    ("private HOME, CARGO_HOME and BARGE_STATE_DIR",
     "n/a for the store surface, implemented as absence: the journal reads no "
     "account facts, so the case environment binds no HOME/CARGO_HOME and no "
     "BARGE_STATE_DIR — it binds BARGE_PROOF_CASE_DIR and scrubs temporaries "
     "under the owned root (case_environment); the staged build is the "
     "ordinary managed Oxide build in the real account, which compiles no "
     "fixture code"),
    ("private account resolution; complete before and after real-state digest",
     "n/a: the driver resolves its fixture state directory from "
     "BARGE_PROOF_CASE_DIR through StatePaths::resolve's explicit override, "
     "never from the account, and no real-state digest is claimed"),
    ("TMPDIR, TMP and TEMP inside the owned root; inherited injection removed",
     "the inner's case environment overrides all three to the case directory "
     "under the owned run root, after base.stage_environment's scrub of "
     "inherited OXIDE, XDG, BARGE and Rust-flag variables; the wall deadline "
     "is minted once by the outer as monotonic_ns and handed to the inner "
     "through BARGE_PROOF_WALL_DEADLINE_NS — one clock domain, never reset; "
     "the remaining time is recomputed fresh before the stage, every case and "
     "publication, and no child is spawned at or past the cutoff"),
    ("explicit fixture volume and area declarations; no real UUID",
     "n/a for images: the journal binds the synthetic fixture UUID a6ac3ed7 "
     "and never verifies a real volume"),
    ("owned images, sequential -owners on attachments and ordinary detach",
     "n/a: journal-only proof, no image is ever attached"),
    ("only the listed proof-only hooks and environment variables",
     "the proof-journal feature's labelled publication-failure endpoints "
     "(operation::fail_next_document_publish, fail_next_watermark_publish); "
     "each injection is named in the case's evidence"),
    ("temporary storage inside the supervisor-owned root",
     "the inner creates its run root, case directories and stage temporaries "
     "inside the guard-supplied OXIDE_TASK_SCRATCH_ROOT, which it verifies "
     "resolves onto the pinned Stash volume inside the designated barge "
     "scratch, owned by this account and free of group/other write bits — the "
     "guard creates it 0755, and the guard's owned root must be private 0700"),
    ("fresh mkdtemp fixture directories under the owned root",
     "every case group's fixture is one fresh mkdtemp directory under the "
     "owned run root"),
    ("qualified supervisor pinned by SHA-256; no second supervisor",
     "the pinned digest below verifies the installed supervisor's bytes in "
     "the unguarded outer half before the one guarded launch"),
)


class Fault(Exception):
    """One recorded fault; counted runs latch it, diagnostics keep going."""

    def __init__(self, kind, detail):
        super().__init__(detail)
        self.kind = kind
        self.detail = detail


# ---------- the external queries ----------
# One real implementation per external query, named at module level. The
# from_environment factories close over exactly these names, so the controls
# never replace a factory: they instantiate the REAL
# `OuterEffects.from_environment()` / `InnerEffects.from_environment()` and
# script only these leaf query points (process, Git, clock, route, storage).
# Anything else — env scrub, artifact parsing, digests, containment, the
# whole composition — runs as it runs in production.

def external_environment():
    return dict(os.environ)


def external_clock_ns():
    return time.monotonic_ns()


def external_run_process(command, env, timeout):
    return subprocess.run(command, env=env, capture_output=True,
                          text=True, timeout=timeout)


def external_run_guarded(command, env):
    return subprocess.run(command, env=env, capture_output=True, text=True)


def external_supervisor_digest():
    digest = hashlib.sha256()
    with open(base.supervisor_path(), "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def external_route_facts():
    return base.scratch_route_facts()


def external_effective_uid():
    return os.getuid()


def external_git(arguments):
    result = subprocess.run(["git", *arguments], cwd=REPO_ROOT,
                            capture_output=True, text=True, timeout=60)
    if result.returncode != 0:
        raise Fault("source",
                    f"git {' '.join(arguments)} failed: {result.stderr[-200:]}")
    return result.stdout.strip()


def external_sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def external_make_directory(parent, prefix):
    return tempfile.mkdtemp(prefix=prefix, dir=parent)


def external_write_file(path, text):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


class OuterEffects:
    """The unguarded entry's external seams: this process's environment, the
    clock (the same monotonic_ns domain the inner reads), the one guarded
    launch, the supervisor digest and the designated route facts."""

    def __init__(self, env, monotonic_ns, run_guarded, supervisor_digest,
                 route_facts):
        self.env = env
        self.monotonic_ns = monotonic_ns
        self.run_guarded = run_guarded
        self.supervisor_digest = supervisor_digest
        self.route_facts = route_facts

    @staticmethod
    def from_environment():
        environment = dict(os.environ)

        return OuterEffects(external_environment(), external_clock_ns,
                            external_run_guarded, external_supervisor_digest,
                            external_route_facts)


class InnerEffects:
    """The guarded body's external seams: the guard-supplied environment, the
    clock, directory creation, the stage and case child executors, the source
    identity and digests, route facts, and receipt publication."""

    def __init__(self, env, monotonic_ns, make_directory, run_process,
                 identity_fn, digest_fn, guard_facts, write_file):
        self.env = env
        self.monotonic_ns = monotonic_ns
        self.make_directory = make_directory
        self.run_process = run_process
        self.identity_fn = identity_fn
        self.digest_fn = digest_fn
        self.guard_facts = guard_facts
        self.write_file = write_file

    def stage(self, env, run_root, timeout):
        """The real stage adapter: the managed Oxide build under the stage
        scrub, bounded by the caller's remaining-time timeout, returning the
        higher-level stage record (selected artifact path + binary digest) —
        never a raw CompletedProcess. The low-level subprocess runner is the
        external seam; this method owns the env scrub, the artifact selection
        and the digest, and bounds every external failure mode (nonzero
        build, timeout, spawn OSError, wrong/duplicate/missing artifact) as
        a Fault so the composition never handles raw process errors."""
        try:
            environment = base.stage_environment(dict(env),
                                                 os.path.join(run_root, "stage-tmp"))
            os.makedirs(environment["TMPDIR"], exist_ok=True)
            result = self.run_process(STAGE_COMMAND, environment, timeout)
            return build_stage_record(result, self.digest_fn)
        except Fault:
            raise
        except subprocess.TimeoutExpired:
            raise Fault("stage",
                        f"the stage build timed out after {timeout:g}s")
        except OSError as error:
            raise Fault("stage", f"the stage build could not start: {error}")

    @staticmethod
    def from_environment():
        environment = external_environment()

        def run_process(command, env, timeout):
            return external_run_process(command, env, timeout)

        def make_directory(parent, prefix):
            return external_make_directory(parent, prefix)

        def identity_fn():
            return source_identity(external_git, external_sha256_file)

        def digest_fn(path):
            return external_sha256_file(path)

        def guard_facts(scratch, evidence):
            """What the host says about the guard-supplied directories, in the
            reference helper's shape: the scratch and evidence facts, the owned
            root that parents them, the verified scratch parent, the scratch
            volume's UUID, and this account's uid. Read-only.

            The facts are taken from the RAW environment paths — one lstat per
            path, never following the leaf — with the real resolution recorded
            beside each. A symlinked path keeps its own `islink` fact and its
            target as `realpath`, so the containment check sees the alias
            instead of silently adopting the target."""
            scratch_real = os.path.realpath(scratch)
            evidence_real = os.path.realpath(evidence)
            route = base.scratch_route_facts(scratch_real, base.SCRATCH_VOLUME_MOUNT)
            owned_root = os.path.dirname(scratch_real)

            def directory_facts(raw_path):
                islink = os.path.islink(raw_path)
                exists = os.path.exists(raw_path)
                info = None
                try:
                    info = os.lstat(raw_path)
                except OSError:
                    pass
                return {
                    "path": raw_path,
                    "realpath": os.path.realpath(raw_path),
                    "present": exists,
                    "isdir": os.path.isdir(raw_path) and not islink,
                    "islink": islink,
                    "uid": info.st_uid if info is not None else None,
                    "mode": info.st_mode & 0o777 if info is not None else None,
                }

            facts = {
                "uid": external_effective_uid(),
                "volume_uuid": route.get("volume_uuid"),
                "path": scratch,
                "root_parent": os.path.realpath(DESIGNATED_SCRATCH),
                "scratch_facts": directory_facts(scratch),
                "evidence_facts": directory_facts(evidence),
                "root_facts": directory_facts(owned_root),
            }
            return facts

        def write_file(path, text):
            external_write_file(path, text)

        return InnerEffects(environment, external_clock_ns, make_directory,
                            run_process, identity_fn, digest_fn,
                            guard_facts, write_file)


def supervisor_path():
    return base.supervisor_path()


def source_identity(git, sha256_file):
    """Bind the actual source the staged build came from."""
    return {
        "head": git(["rev-parse", "HEAD"]),
        "tree": git(["rev-parse", "HEAD^{tree}"]),
        "cargo_lock_sha256": sha256_file(CARGO_LOCK),
        "toolchain_sha256": sha256_file(TOOLCHAIN_FILE),
        "toolchain_channel": TOOLCHAIN_CHANNEL,
        "features": [PROOF_FEATURE],
        "stage_command": " ".join(STAGE_COMMAND),
        "driver_source_sha256": sha256_file(DRIVER_SOURCE),
        "store_source_sha256": sha256_file(STORE_SOURCE),
    }


SOURCE_IDENTITY_FIELDS = ("head", "tree", "cargo_lock_sha256", "toolchain_sha256",
                          "driver_source_sha256", "store_source_sha256")


def identity_drift(first, second):
    """The source fields two identities must agree on, and where they do not."""
    return [field for field in SOURCE_IDENTITY_FIELDS
            if first.get(field) != second.get(field)]


def manifest_complete(manifest):
    """Why an inner receipt's manifest is not a complete source binding."""
    problems = []
    required = ("head", "tree", "cargo_lock_sha256", "toolchain_sha256",
                "driver_source_sha256", "store_source_sha256", "binary_sha256")
    for field in required:
        value = manifest.get(field)
        if not isinstance(value, str) or len(value) < 32:
            problems.append(f"the manifest's {field} is missing or not a digest")
    if manifest.get("features") != [PROOF_FEATURE]:
        problems.append("the manifest does not carry exactly the proof feature")
    return problems


def case_environment(inner_env, case_dir):
    """The case child's environment: the stage scrub, then the fixture
    bindings applied after it, so the scrub cannot drop a binding. The
    journal surface reads no account facts, so no HOME is exported — the
    temporaries and the case directory are the bindings that exist."""
    scrubbed = base.stage_environment(dict(inner_env), case_dir)
    scrubbed.update({
        "BARGE_PROOF_CASE_DIR": case_dir,
        "TMPDIR": case_dir,
        "TMP": case_dir,
        "TEMP": case_dir,
    })
    return scrubbed


def inner_command(mode):
    """The guarded inner argv: this script's inner runner, bound to the mode
    alone. The inner stages and binds the source itself, inside the guard,
    from the guard-created environment."""
    return ["python3", SCRIPT_PATH, "--inner", mode]


def supervisor_command(inner_argv):
    """The unchanged qualified supervisor wrapping the inner argv: the one
    guarded child a native run spawns, rooted at the designated Stash scratch
    the outer verified."""
    return [
        sys.executable, base.supervisor_path(),
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", DESIGNATED_SCRATCH,
        "--", *inner_argv,
    ]


def designated_route_problems(facts):
    """Why the designated scratch is not the verified private Stash route."""
    problems = []
    if facts.get("path") != DESIGNATED_SCRATCH:
        problems.append(f"the designated scratch is {facts.get('path')!r}, "
                        f"not {DESIGNATED_SCRATCH!r}")
    if facts.get("present") is not True:
        problems.append(f"the designated scratch {facts.get('path')!r} is absent")
    if facts.get("realpath") != facts.get("path"):
        problems.append(f"the designated scratch resolves to {facts.get('realpath')!r}, "
                        "so its path is not canonical")
    if facts.get("volume_uuid") != ROUTE_UUID:
        problems.append(
            f"the designated scratch's volume is {facts.get('volume_uuid')}, "
            f"not the verified private Stash {ROUTE_UUID}"
        )
    if facts.get("owner_uid") is not None and facts.get("uid") is not None \
            and facts.get("owner_uid") != facts.get("uid"):
        problems.append("the designated scratch is not owned by this account")
    mode = facts.get("mode")
    if isinstance(mode, int) and mode & 0o077:
        problems.append(f"the designated scratch mode {mode!r} is not owner-only")
    return problems


def _guard_private_directory_problems(name, facts, uid, owner_only=False):
    """Why one guard-set directory is not this occurrence's canonical directory.

    Mirrors the reference helper: a symlink, a non-directory, a non-absolute or
    non-canonical path, another account's ownership, or group/other write bits
    (owner_only: any group/other bit at all) each refuse. The pinned supervisor
    creates the scratch and evidence directories 0755, so ordinary read/
    execute bits for group and other are accepted — ownership is the boundary,
    never the read bits."""
    problems = []
    if isinstance(facts, dict) and facts.get("islink") is True:
        return [f"the {name} {facts.get('path')!r} is a symlink"]
    if not isinstance(facts, dict) or facts.get("present") is not True \
            or facts.get("isdir") is not True:
        return [f"the {name} is not a directory"]
    path = facts.get("path")
    if not isinstance(path, str) or not os.path.isabs(path) \
            or os.path.normpath(path) != path:
        problems.append(f"the {name} {path!r} is not an absolute canonical path")
    if facts.get("realpath") != path:
        problems.append(f"the {name} {path!r} resolves to {facts.get('realpath')!r}, "
                        "so its path is an alias or a symlink")
    if facts.get("uid") != uid:
        problems.append(f"the {name} {path!r} is not owned by this account")
    mode = facts.get("mode")
    writable = 0o077 if owner_only else 0o022
    if not isinstance(mode, int) or mode & writable:
        problems.append(f"the {name} {path!r} mode {mode!r} is not private")
    return problems


def guard_owned_root(scratch_path):
    """The guard's monitored owned root: the parent of the scratch directory
    the supervisor sets (the pinned bytes create one `oxide-proof.*` root
    under `--root-parent` and then `scratch/` inside it)."""
    if not isinstance(scratch_path, str) or not os.path.isabs(scratch_path):
        return None
    parent = os.path.dirname(os.path.normpath(scratch_path))
    return parent if parent not in ("", os.sep) else None


def guard_containment_problems(facts):
    """Why the guard-created layout is not this occurrence's monitored one.

    Mirrors the reference helper: the scratch and evidence directories must be
    canonical, un-symlinked, directories, owned by this account and free of
    group/other write bits (the guard creates them 0755); the owned root that
    parents them must be a direct child of the verified designated scratch and
    private 0700; the evidence must live inside that owned root, must not
    alias it or the scratch; and the scratch's volume must be the verified
    private Stash UUID. A layout the pinned supervisor would not have created
    refuses, and one it does create — 0755 scratch and evidence over a 0700
    owned root — passes."""
    problems = []
    uid = facts.get("uid")
    root_parent = facts.get("root_parent")
    scratch = facts.get("scratch_facts") or {}
    problems += _guard_private_directory_problems(
        "scratch directory", scratch, uid, owner_only=False)
    problems += _guard_private_directory_problems(
        "evidence directory", facts.get("evidence_facts"), uid, owner_only=False)
    problems += _guard_private_directory_problems(
        "owned root", facts.get("root_facts"), uid, owner_only=True)
    scratch_path = scratch.get("path") if isinstance(scratch, dict) else None
    owned_root = guard_owned_root(scratch_path)
    if not owned_root:
        problems.append("the scratch directory does not name a guarded owned root")
    else:
        if os.path.dirname(owned_root) != root_parent:
            problems.append(f"the owned root {owned_root} is not directly under "
                            f"the verified scratch parent {root_parent}")
    evidence = facts.get("evidence_facts")
    evidence_path = evidence.get("path") if isinstance(evidence, dict) else None
    if not evidence_path:
        problems.append("the evidence directory is absent")
    else:
        if owned_root and evidence_path == owned_root:
            problems.append("the evidence directory names the owned root itself")
        scratch_real = scratch.get("realpath") or ""
        if owned_root and scratch_real \
                and os.path.normpath(evidence_path) == os.path.normpath(scratch_real):
            problems.append("the evidence directory aliases the scratch directory")
        # The evidence must live inside the monitored owned root — by
        # resolution, so an escaping alias is caught even when its raw path
        # names a point inside the root.
        evidence_real = evidence.get("realpath") or ""
        if owned_root and evidence_real:
            if evidence_real != owned_root \
                    and not evidence_real.startswith(owned_root + os.sep):
                problems.append(f"the evidence directory {evidence.get('path')!r} "
                                f"is outside the guard's owned root {owned_root!r}")
    if facts.get("volume_uuid") != ROUTE_UUID:
        problems.append(
            f"the scratch volume is {facts.get('volume_uuid')}, "
            f"not the verified private Stash {ROUTE_UUID}"
        )
    return problems


def build_stage_record(result, digest_fn):
    """The real artifact parser and hasher for one managed build result.

    Selects exactly one `journal_proof` example artifact carrying exactly the
    proof feature, and returns the higher-level stage record: the staged
    binary path and its SHA-256. Never a raw CompletedProcess."""
    if result.returncode != 0:
        raise Fault("stage",
                    f"the stage build exited {result.returncode}: {result.stderr[-400:]}")
    matches = []
    for line in result.stdout.splitlines():
        try:
            document = json.loads(line)
        except ValueError:
            continue
        target = document.get("target") or {}
        if (document.get("reason") == "compiler-artifact"
                and "example" in (target.get("kind") or [])
                and target.get("name") == DRIVER_NAME
                and document.get("executable")):
            matches.append(document)
    if not matches:
        raise Fault("stage", "the stage build produced no journal_proof artifact")
    if len(matches) > 1:
        raise Fault("stage",
                    f"the stage build produced {len(matches)} journal_proof artifacts")
    selected = matches[0]
    if selected.get("features") != [PROOF_FEATURE]:
        raise Fault("stage", "the staged artifact does not carry exactly the proof feature")
    binary = selected["executable"]
    return {"binary": binary, "binary_sha256": digest_fn(binary)}


def wall_deadline_ns(effects):
    """The absolute monotonic wall deadline the outer minted, in ns.

    One clock domain: `time.monotonic_ns()` in both halves. The guard's raw
    CLOCK_UPTIME_RAW ns (OXIDE_PROOF_DEADLINE_NS) is the supervisor's own
    group deadline and is never subtracted from another clock. Missing or
    malformed is a refusal in real inner mode — never an unbudgeted
    fallback."""
    raw = effects.env.get(base.WALL_DEADLINE_ENV)
    try:
        value = int(raw)
    except (TypeError, ValueError):
        raise Fault("deadline",
                    f"{base.WALL_DEADLINE_ENV} is missing or malformed; "
                    "this is not the outer's guarded run") from None
    if value <= 0:
        raise Fault("deadline",
                    f"{base.WALL_DEADLINE_ENV} does not name a positive wall deadline")
    return value


def remaining_seconds(effects, deadline_ns):
    """The freshly measured remaining wall time in seconds, never cached."""
    return (deadline_ns - effects.monotonic_ns()) / 1e9


def spawn_allowed(remaining):
    """Whether a new child may start: strictly before the closure margin."""
    return remaining > WORK_STOP_SECONDS


def child_timeout(remaining, base_timeout):
    """The executor timeout capped by the remaining wall minus the margin."""
    return min(base_timeout, max(0.0, remaining - WORK_STOP_SECONDS))


def run_cases_inner(mode, run_root, effects, deadline_ns):
    """The guarded inner composition: capture the pre-stage identity, stage
    the proof build inside the guard (spawning only while the freshly
    measured remaining wall time exceeds the closure margin, with the
    executor timeout capped by that remainder), mint the source-bound
    manifest and require it to equal the pre-stage identity (drift refuses),
    re-hash the staged binary before the first case, and run the twelve fixed
    case groups — counted fail-fast or diagnostic keep-going — then return
    one inner receipt for publication into the guard's evidence directory.

    This is the function the `--inner` runner calls with real child
    execution, and the effect-free controls call with only the external
    subprocess/Git/clock/storage seams faked — the composition itself,
    including the real stage adapter, the manifest mint and the drift
    comparison, is never a fake.
    """
    cases = []
    passed = set()
    fault = None
    stage_record = None
    manifest = None
    pre_stage = None
    stage_blocked_detail = None
    # The pre-stage identity is captured BEFORE the stage in every path: the
    # manifest minted after the build must equal it, so a checkout change
    # during the build is drift and refuses. The capture itself is bounded:
    # a source seam failure (the real git query refusing, or any other
    # error) lands as a bounded source fault with its receipt — it never
    # escapes this composition without evidence.
    try:
        pre_stage = effects.identity_fn()
    except Fault as error:
        fault = error
    except Exception as error:  # noqa: BLE001 - bounded by contract
        fault = Fault("source",
                      f"the pre-stage source identity could not be read: "
                      f"{type(error).__name__}: {error}")

    if fault is None:
        remaining = remaining_seconds(effects, deadline_ns)
        stage_blocked_detail = None
        if not spawn_allowed(remaining):
            stage_blocked_detail = "no guarded wall left to stage inside the closure margin"
        else:
            try:
                stage_record = effects.stage(env=effects.env, run_root=run_root,
                                             timeout=child_timeout(remaining, STAGE_BASE_TIMEOUT))
                manifest = effects.identity_fn()
                drifts = identity_drift(pre_stage, manifest)
                if drifts:
                    raise Fault("drift",
                                f"the checkout changed during the in-guard stage: "
                                f"{', '.join(drifts)}")
                manifest["binary_sha256"] = stage_record["binary_sha256"]
                incomplete = manifest_complete(manifest)
                if incomplete:
                    raise Fault("manifest", "; ".join(incomplete))
                # The staged pool binary is re-hashed before the first case: a
                # replaced binary is drift, never the manifest's evidence.
                if effects.digest_fn(stage_record["binary"]) != stage_record["binary_sha256"]:
                    raise Fault("drift", "the staged binary changed after the stage")
            except Fault as error:
                fault = error
            except Exception as error:  # noqa: BLE001 - bounded by contract
                fault = Fault("stage",
                              f"the stage adapter failed unexpectedly: {error!r}")

    if fault is None:
        for case in CASES:
            remaining = remaining_seconds(effects, deadline_ns)
            if not spawn_allowed(remaining):
                cases.append({"case": case["id"], "status": "unexecuted",
                              "detail": "inside the closure margin or past the wall"})
                continue
            if fault and mode == "counted":
                cases.append({"case": case["id"], "status": "skipped",
                              "kind": "latched",
                              "detail": "a counted run latched an earlier fault"})
                continue
            case_dir = effects.make_directory(run_root, f"case-{case['id']}-")
            if not os.path.realpath(case_dir).startswith(run_root + os.sep):
                fault = Fault("custody", "a case directory escaped the owned run root")
                cases.append({"case": case["id"], "status": "failed",
                              "kind": "custody", "detail": fault.detail})
                continue
            try:
                command = [stage_record["binary"], case["id"]]
                result = effects.run_process(
                    command, case_environment(effects.env, case_dir),
                    child_timeout(remaining, CASE_BASE_TIMEOUT))
                verdicts = []
                for line in result.stdout.splitlines():
                    try:
                        document = json.loads(line)
                    except ValueError:
                        continue
                    if not isinstance(document, dict) or "case" not in document:
                        continue
                    if document.get("case") == case["id"]:
                        verdicts.append(document)
                    else:
                        raise Fault("child",
                                    f"case {case['id']}: the child answered for unknown case "
                                    f"{document.get('case')!r}")
                if not verdicts:
                    raise Fault("child",
                                f"case {case['id']}: no verdict in the child's output "
                                f"(exit {result.returncode}); "
                                f"stdout tail: {result.stdout[-200:]}")
                if len(verdicts) > 1:
                    raise Fault("child",
                                f"case {case['id']}: the child answered {len(verdicts)} times")
                verdict = verdicts[0]
                if result.returncode != 0 or verdict.get("status") != "passed":
                    raise Fault("case",
                                f"case {case['id']} failed: status={verdict.get('status')!r} "
                                f"exit={result.returncode} "
                                f"detail={str(verdict.get('detail', ''))[:300]}")
                passed.add(case["id"])
                cases.append({"case": case["id"], "status": "passed",
                              "evidence": {"detail": verdict.get("detail", ""),
                                           "injections": verdict.get("injections", []),
                                           "case_dir": case_dir}})
            except Fault as error:
                fault = error
                cases.append({"case": case["id"], "status": "failed",
                              "kind": error.kind, "detail": error.detail})
            except Exception as error:  # noqa: BLE001 - recorded, never hidden
                fault = Fault("case", f"{type(error).__name__}: {error}")
                cases.append({"case": case["id"], "status": "failed",
                              "kind": "case", "detail": str(error)})

    if fault is not None and not any(case.get("case") for case in cases):
        # The stage faulted before any case ran: the counted receipt still
        # accounts for every fixed group, recorded as never executed.
        recorded = {case.get("case") for case in cases}
        for case in CASES:
            if case["id"] not in recorded:
                cases.append({"case": case["id"], "status": "unexecuted",
                              "detail": "the stage faulted before this case"})
        cases.sort(key=lambda case: case.get("case") or "")

    remaining_at_publication = remaining_seconds(effects, deadline_ns)
    clean = (mode == "counted" and fault is None and len(passed) == len(CASES)
             and len(cases) == len(CASES))
    receipt = {
        "schema": SCHEMA,
        "stage": "inner",
        "mode": mode,
        "pre_stage_identity": dict(pre_stage) if isinstance(pre_stage, dict) else None,
        "manifest": manifest,
        "run_root": run_root,
        "qualified_inner": bool(clean),
        "passed": sorted(passed),
        "cases": cases,
        "remaining_seconds_at_publication": round(remaining_at_publication, 3),
    }
    if fault is not None:
        receipt["fault"] = {"kind": fault.kind, "detail": fault.detail}
    if stage_blocked_detail:
        receipt["stage_unexecuted"] = stage_blocked_detail
    return receipt


def inner_half(mode, effects):
    """The guarded inner entry: validate that this really is the pinned
    supervisor's guarded run, create the fixture and stage roots inside the
    verified owned scratch, run the composition, write the receipt into the
    guard's evidence directory before any cleanup, and return it. Refusals
    print the bounded document and exit 125 with no fixture effect."""
    refusals = []
    scratch = effects.env.get("OXIDE_TASK_SCRATCH_ROOT")
    evidence = effects.env.get("OXIDE_PROOF_EVIDENCE")
    if not scratch:
        refusals.append("OXIDE_TASK_SCRATCH_ROOT is unset; this is not the guard's run")
    if not evidence:
        refusals.append("OXIDE_PROOF_EVIDENCE is unset; this is not the guard's run")
    try:
        deadline_ns = wall_deadline_ns(effects)
    except Fault as error:
        refusals.append(error.detail)
    if refusals:
        return {"schema": SCHEMA, "stage": "inner", "mode": mode,
                "qualified_inner": False, "cases": [], "refused": refusals}
    facts = effects.guard_facts(scratch, evidence)
    facts["path"] = scratch
    containment = guard_containment_problems(facts)
    if containment:
        return {"schema": SCHEMA, "stage": "inner", "mode": mode,
                "qualified_inner": False, "cases": [], "refused": containment}
    scratch_real = (facts.get("scratch_facts") or {}).get("realpath") or scratch

    try:
        run_root = effects.make_directory(scratch_real, RUN_ROOT_PREFIX)
    except OSError as error:
        return {"schema": SCHEMA, "stage": "inner", "mode": mode,
                "qualified_inner": False, "cases": [],
                "refused": [f"the run root could not be created inside the "
                            f"owned scratch: {error}"]}
    if not os.path.realpath(run_root).startswith(scratch_real + os.sep):
        return {"schema": SCHEMA, "stage": "inner", "mode": mode,
                "qualified_inner": False, "cases": [],
                "refused": ["the run root escaped the guard's owned scratch"]}

    receipt = run_cases_inner(mode, run_root, effects, deadline_ns)
    receipt["refused"] = []
    receipt_path = os.path.join(os.path.realpath(evidence), RECEIPT_NAME)
    try:
        effects.write_file(receipt_path, json.dumps(receipt, indent=2) + "\n")
        receipt["receipt_path"] = receipt_path
    except OSError as error:
        receipt["receipt_write_fault"] = str(error)
        receipt["qualified_inner"] = False
    return receipt


def outer_half(mode, effects):
    """The unguarded outer entry: verify the pinned supervisor's bytes and
    the designated Stash route metadata, mint the wall deadline once, launch
    exactly one guarded child, and return the actual guard exit — never a
    post-guard guessed receipt through stale env. No guard-created variable
    is read or required here; no fixture, stage root or receipt is created
    outside the guard."""
    receipt = {"schema": SCHEMA, "stage": "outer", "mode": mode,
               "qualified": False,
               "supervisor_sha256": base.SUPERVISOR_SHA256,
               "supervisor_verified": False, "route_uuid": ROUTE_UUID,
               "route_verified": False, "guard_exit": None,
               "inner": None, "manifest": None, "cases": [],
               "bounds": {"wall_seconds": LIMIT_SECONDS,
                          "scratch_mib": SCRATCH_BUDGET_MIB, "image": None,
                          "work_stop_seconds": WORK_STOP_SECONDS,
                          "fit": "unmeasured"}}
    try:
        found = effects.supervisor_digest()
    except OSError as error:
        receipt["refused"] = f"the pinned supervisor is unreadable: {error}"
        print(json.dumps(receipt))
        return 125, receipt
    if found != base.SUPERVISOR_SHA256:
        receipt["refused"] = (f"the supervisor's bytes are {found}, "
                              f"not the pinned {base.SUPERVISOR_SHA256}")
        return 125, receipt
    receipt["supervisor_verified"] = True
    route_faults = designated_route_problems(effects.route_facts())
    if route_faults:
        receipt["refused"] = route_faults
        return 125, receipt
    receipt["route_verified"] = True

    # One clock domain: time.monotonic_ns() in both halves. The wall deadline
    # is minted here exactly once, with two seconds reserved, and the inner
    # never resets it. The guard-created variables are never inherited: they
    # do not exist until the supervisor creates them for its guarded child.
    env = dict(effects.env)
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env.pop("BARGE_PROOF_ROUTE", None)
    env[base.WALL_DEADLINE_ENV] = str(effects.monotonic_ns()
                                      + (LIMIT_SECONDS - 2) * 1_000_000_000)
    env["BARGE_PROOF_SUPERVISOR"] = supervisor_path()
    try:
        guarded = effects.run_guarded(supervisor_command(inner_command(mode)), env)
    except subprocess.TimeoutExpired:
        receipt["refused"] = ["the guarded supervisor child outlived this process"]
        return 1, receipt
    receipt["guard_exit"] = guarded.returncode

    inner = None
    for line in guarded.stdout.splitlines():
        try:
            document = json.loads(line)
        except ValueError:
            continue
        if isinstance(document, dict) and document.get("schema") == SCHEMA \
                and document.get("stage") == "inner":
            if inner is not None:
                receipt["refused"] = ["the guarded child answered more than once"]
                return 1, receipt
            inner = document
    if inner is None:
        receipt["refused"] = [f"the guarded child printed no inner receipt "
                              f"(exit {guarded.returncode}); stdout tail: "
                              f"{guarded.stdout[-300:]}"]
        return 1, receipt
    receipt["inner"] = inner
    receipt["manifest"] = inner.get("manifest")
    receipt["cases"] = inner.get("cases", [])
    if inner.get("fault") and not inner.get("cases"):
        # A faulted inner receipt is a legitimate recorded outcome: the outer
        # composes it as the failure it is, never qualified.
        receipt["fault"] = inner["fault"]
    else:
        seen = [case.get("case") for case in receipt["cases"]]
        expected = [case["id"] for case in CASES]
        if sorted(seen) != sorted(expected):
            receipt["refused"] = [f"the inner receipt's cases {seen} are not "
                                  f"exactly the fixed map {expected}"]
        elif not receipt["manifest"]:
            receipt["refused"] = ["the inner receipt carries no manifest"]
        else:
            incomplete = manifest_complete(receipt["manifest"])
            if incomplete:
                receipt["refused"] = incomplete
        if receipt.get("refused"):
            return 1, receipt
    receipt["qualified"] = bool(
        mode == "counted" and guarded.returncode == 0
        and inner.get("qualified_inner") is True
        and receipt["supervisor_verified"] and receipt["route_verified"]
        and not receipt.get("refused") and not receipt.get("fault")
    )
    exit_code = guarded.returncode if (receipt["qualified"] or mode == "diagnostic") \
        else max(1, guarded.returncode)
    return exit_code, receipt


RECEIPT_LINE_BOUND = 65536


def bounded_receipt_line(receipt):
    """One bounded compact JSON line carrying the inner receipt: the guarded
    child's stdout handoff to the unguarded outer. A document that exceeds
    the bound falls back to a small honest refusal rather than an unparseable
    or unbounded line."""
    line = json.dumps(receipt, separators=(",", ":"))
    if len(line) <= RECEIPT_LINE_BOUND:
        return line
    bounded = {"schema": SCHEMA, "stage": "inner",
               "mode": receipt.get("mode"), "qualified_inner": False,
               "fault": {"kind": "receipt",
                         "detail": "the inner receipt exceeded the bounded "
                                   "stdout line"}}
    return json.dumps(bounded, separators=(",", ":"))


def run_inner(arguments):
    """The guarded inner runner: validate the guard-supplied run, compose the
    cases, publish the receipt into the guard's evidence, and hand ONE
    bounded JSON receipt line back through stdout — on success and on fault
    alike, so the unguarded outer can parse what actually happened. Exit
    zero only for a clean counted pass."""
    effects = InnerEffects.from_environment()
    receipt = inner_half(arguments.inner, effects)
    line = bounded_receipt_line(receipt)
    clean = (arguments.inner == "counted" and receipt.get("qualified_inner")
             and not receipt.get("refused") and not receipt.get("fault"))
    return (0 if clean else 1), line


# ---------- effect-free controls through the actual entries ----------

class ControlHarness:
    """The scripted external world for the actual `outer_half` and
    `inner_half`.

    O1: the harness's outer environment carries no guard-created variables,
    so `outer_half` must run exactly as the genuine Effigy entry would. Its
    one guarded launch is "executed" by really running the actual
    `inner_half` — through the REAL `InnerEffects.from_environment()`
    factory over the guard-constructed child environment — with only the
    leaf external queries scripted: the subprocess runner (raw Cargo artifact
    JSON and raw case verdicts), Git, the clock, the supervisor/route
    queries, directory creation and receipt publication. No factory is ever
    replaced and no stage record is ever faked.

    Every harness owns one fresh private per-run fixture directory under
    this run's own TMPDIR; while its `real_factory()` context is held, the
    module's DESIGNATED_SCRATCH points at that fixture's designated path, so
    the real guard layout — and everything the real stage adapter and the
    run root create — stays inside the harness's own fixture. The shared
    designated scratch root is never touched."""

    def __init__(self):
        self.env = {"HOME": "/home", "TMPDIR": "/tmp", "PATH": "/bin"}
        self.sequence = []
        self.stage_timeouts = []
        self.case_timeouts = []
        self.written = {}
        self.directories = []
        self.monotonic_ns_value = 1_000_000_000_000
        self.clock = None
        self.supervisor_digest_value = base.SUPERVISOR_SHA256
        self.route_volume_uuid = ROUTE_UUID
        self.supply_guard_env = True
        self.strip_wall_deadline = False
        self.wall_deadline_override = None
        self.scratch_override = None
        self.evidence_override = None
        self.stage_fault = None
        self.stage_raise = None
        self.build_returncode = 0
        self.build_calls = []
        self.digest_calls = []
        self.case_results = {}
        self.identity_drift_after_capture = False
        self.replaced_binary_digest = None
        self.guard_exit_override = None
        self.make_directory_parent = None
        self.case_directory_parent = None
        self.fail_report_write = False
        self.stage_output = ('{"reason":"compiler-artifact","target":{"kind":["example"],'
                             '"crate_types":["bin"],"name":"' + DRIVER_NAME + '","edition":"2021"},'
                             '"features":["' + PROOF_FEATURE + '"],'
                             '"executable":"/stage/' + DRIVER_NAME + '"}\n')
        self.head = "0" * 40
        self.source_digests = {DRIVER_SOURCE: "d" * 64, STORE_SOURCE: "s" * 64,
                               CARGO_LOCK: "l" * 64, TOOLCHAIN_FILE: "t" * 64}
        self.last_guarded_command = None
        self.last_guarded_env = None
        self.guarded_launches = 0
        self.used_designated = None
        self.git_calls = 0
        self.inner_receipt = None
        # One fresh private per-run fixture under this run's own TMPDIR, with
        # the real guard layout materialized inside it exactly as the pinned
        # supervisor creates it: 0755 scratch and evidence over a 0700 owned
        # owned root, owned by this account. The REAL reader reads it.
        self._tmp = tempfile.TemporaryDirectory(prefix="journal-control-")
        self.fixture_root = os.path.realpath(self._tmp.name)
        self.saved_designated = DESIGNATED_SCRATCH
        self.fixture_designated = os.path.join(self.fixture_root, "designated")
        self.fixture_guard_root = os.path.join(self.fixture_designated,
                                               "journal-guard")
        self.fixture_scratch = os.path.join(self.fixture_guard_root, "scratch")
        self.fixture_evidence = os.path.join(self.fixture_guard_root, "evidence")
        os.makedirs(self.fixture_scratch)
        os.makedirs(self.fixture_evidence)
        os.chmod(self.fixture_guard_root, 0o700)

    def cleanup(self):
        self._tmp.cleanup()

    # The scripted external queries (the only fakes)
    def supervisor_digest(self):
        return self.supervisor_digest_value

    def route_facts(self):
        designated = DESIGNATED_SCRATCH
        return {"path": designated, "realpath": designated,
                "volume_uuid": self.route_volume_uuid,
                "owner_uid": os.getuid(), "uid": os.getuid(),
                "mode": 0o700, "present": True}

    def run_guarded(self, command, env):
        """The scripted guard boundary: the guard constructs the child's
        environment (scratch, evidence, wall deadline) and launches; the
        child process then really runs `inner_half` over the REAL
        `InnerEffects.from_environment()` factory built from that
        environment."""
        self.last_guarded_command = list(command)
        self.last_guarded_env = dict(env)
        self.writes_before_launch = len(self.written)
        self.directories_before_launch = len(self.directories)
        self.sequence.append("guarded")
        self.used_designated = DESIGNATED_SCRATCH
        self.guarded_launches += 1
        inner_mode = command[command.index("--") + 1:][-1]
        inner_env = dict(env)
        if self.supply_guard_env:
            layout = self.guard_layout()
            inner_env["OXIDE_TASK_SCRATCH_ROOT"] = layout["scratch"]
            inner_env["OXIDE_PROOF_EVIDENCE"] = layout["evidence"]
        if self.strip_wall_deadline:
            inner_env.pop(base.WALL_DEADLINE_ENV, None)
        elif self.wall_deadline_override is not None:
            inner_env[base.WALL_DEADLINE_ENV] = self.wall_deadline_override
        saved_env = self.env
        self.env = inner_env
        try:
            receipt = inner_half(inner_mode, InnerEffects.from_environment())
        finally:
            self.env = saved_env
        self.inner_receipt = receipt
        clean = (inner_mode == "counted" and receipt.get("qualified_inner")
                 and not receipt.get("refused") and not receipt.get("fault"))
        exit_code = self.guard_exit_override if self.guard_exit_override is not None \
            else (0 if clean else 1)
        return types.SimpleNamespace(returncode=exit_code,
                                     stdout=json.dumps(receipt) + "\n", stderr="")

    def guard_layout(self):
        """The guard-supplied bindings for this run: inside this harness's
        own fixture unless a control overrides a path to exercise a real
        containment refusal."""
        scratch = self.scratch_override or \
            (DESIGNATED_SCRATCH + "/journal-guard/scratch")
        evidence = self.evidence_override or \
            (DESIGNATED_SCRATCH + "/journal-guard/evidence")
        return {"scratch": scratch, "evidence": evidence}

    def monotonic_ns(self):
        if self.clock and len(self.clock) > 1:
            return self.clock.pop(0)
        if self.clock:
            return self.clock[0]
        return self.monotonic_ns_value

    def make_directory(self, parent, prefix):
        """The scripted storage query: a real directory under the given
        parent, recorded; a control's custody knob can point a case
        directory outside the run root, virtually, to exercise the
        containment refusal without a stray write."""
        if prefix.startswith("case-") and self.case_directory_parent:
            path = f"{self.case_directory_parent}/{prefix}{len(self.directories)}"
            self.directories.append(path)
            return path
        if self.make_directory_parent:
            parent = self.make_directory_parent
        path = tempfile.mkdtemp(prefix=prefix, dir=parent)
        self.directories.append(path)
        return path

    def run_process(self, command, env, timeout):
        """The low-level subprocess seam: every real child (the stage build
        and each driver case) arrives here as `(command, env, timeout)` and
        leaves as a CompletedProcess-shaped namespace. The fake supplies raw
        Cargo artifact JSON and raw case verdicts — never a stage record."""
        if command[:2] == ["oxide", "build"]:
            self.sequence.append("stage")
            self.stage_timeouts.append(timeout)
            self.build_calls.append({"command": list(command), "env": dict(env),
                                     "timeout": timeout})
            if self.stage_raise is not None:
                raise self.stage_raise
            if self.stage_fault:
                raise Fault(self.stage_fault[0], self.stage_fault[1])
            return types.SimpleNamespace(returncode=self.build_returncode,
                                         stdout=self.stage_output, stderr="")
        case = command[-1]
        self.sequence.append(f"case:{case}")
        self.case_timeouts.append(timeout)
        scripted = self.case_results.get(case)
        if scripted is not None:
            return scripted
        return types.SimpleNamespace(
            returncode=0,
            stdout=json.dumps({"case": case, "status": "passed",
                               "detail": "fake pass", "injections": []}) + "\n",
            stderr="")

    def git(self, arguments):
        self.git_calls += 1
        return self.head

    def sha256_file(self, path):
        """The scripted digest query: real files (binary-as-data) are hashed
        for real, source files carry scripted digests, and the control knobs
        — replaced binary at the re-hash, identity drift after the pre-stage
        capture — fire only at their named moments."""
        self.digest_calls.append(path)
        if (self.identity_drift_after_capture
                and path == STORE_SOURCE and self.git_calls >= 4):
            # The second identity capture (git calls 3 and 4 are the manifest
            # mint's rev-parses) observes a changed store source. This knob
            # precedes real-file hashing: it scripts the drift semantically.
            return "changed" * 8
        if os.path.isfile(path):
            with open(path, "rb") as handle:
                return hashlib.sha256(handle.read()).hexdigest()
        if path == "/stage/" + DRIVER_NAME:
            if (self.replaced_binary_digest is not None
                    and self.digest_calls.count(path) >= 2):
                # Only the pre-first-case re-hash observes the replacement:
                # the stage-time digest stays the original binary's.
                return self.replaced_binary_digest
            return "f" * 64
        return self.source_digests.get(path, "a" * 64)

    def write_file(self, path, text):
        if self.fail_report_write:
            raise OSError("the injected report failure")
        self.written[path] = text

    @contextlib.contextmanager
    def real_factory(self):
        """Install this harness as the process's external world for the
        enclosed block and point the module's designated scratch at this
        harness's own private fixture. The `from_environment` factories stay
        real; only these leaf external queries are scripted."""
        saved = {}

        def install(name, value):
            saved[name] = globals()[name]
            globals()[name] = value

        install("external_environment", lambda: dict(self.env))
        install("external_clock_ns", self.monotonic_ns)
        install("external_run_process", self.run_process)
        install("external_run_guarded", self.run_guarded)
        install("external_supervisor_digest", self.supervisor_digest)
        install("external_route_facts", self.route_facts)
        install("external_git", self.git)
        install("external_sha256_file", self.sha256_file)
        install("external_make_directory", self.make_directory)
        install("external_write_file", self.write_file)
        install("DESIGNATED_SCRATCH", self.fixture_designated)
        try:
            yield self
        finally:
            for name, value in saved.items():
                globals()[name] = value


def control_harness():
    return ControlHarness()


def run_outer(harness, mode="counted"):
    """Drive the REAL unguarded outer over the REAL factories: the harness
    is installed as the process's external world for the call, so
    `OuterEffects.from_environment()` and the guard child's
    `InnerEffects.from_environment()` are both the production factories and
    only the leaf external queries are scripted. The harness is owned here:
    its fixture is closed deterministically when the run returns, never
    left to the garbage collector."""
    try:
        with harness.real_factory():
            return outer_half(mode, OuterEffects.from_environment())
    finally:
        harness.cleanup()


def all_cases_unexecuted(receipt):
    """Whether a faulted receipt accounts for every fixed group as honestly
    unexecuted (never vacuously: the row count is checked too)."""
    cases = (receipt or {}).get("cases") or []
    return (len(cases) == len(CASES)
            and all(case.get("status") == "unexecuted" for case in cases))


def composition_controls():
    """Drive the actual `outer_half` and `inner_half` entries through the
    REAL `from_environment` factories: the unguarded launch with no guard
    env, the guard-supplied inner bindings, the fresh remaining-time
    behavior, and the failure paths — with only the external process, Git,
    clock, route and storage queries scripted. Every fixture is a fresh
    private per-run directory under this run's own TMPDIR; the shared
    designated scratch root is never created in, written, or removed."""
    problems = []

    def expect(condition, message):
        if not condition:
            problems.append(message)

    # O1, normal unguarded entry: no guard-created env keys, one guarded
    # launch, zero fixture/stage/receipt writes before it, genuine hooks
    # preserved, and the inner really ran inside the guarded child over the
    # real inner factory. The run's designated scratch is this harness's own
    # private fixture — provably not the shared root.
    harness = control_harness()
    receipt_exit, outer_receipt = run_outer(harness)
    expect(isinstance(receipt_exit, int) and receipt_exit == 0,
           "a clean counted pass did not exit zero")
    expect(outer_receipt.get("qualified") is True,
           "the outer receipt did not qualify the clean pass")
    expect(harness.guarded_launches == 1, "the outer did not launch exactly one guard")
    expect(harness.sequence[0] == "guarded", "the launch was not the first action")
    expect(harness.writes_before_launch == 0,
           "the outer wrote a receipt or provenance before the guard launch")
    expect(harness.directories_before_launch == 0,
           "the outer created a fixture or stage root before the guard launch")
    expect("OXIDE_TASK_SCRATCH_ROOT" not in harness.last_guarded_env,
           "the outer fabricated or inherited a guard scratch root")
    expect(base.WALL_DEADLINE_ENV in harness.last_guarded_env,
           "the outer did not mint the wall deadline for the guarded child")
    expect(harness.last_guarded_env.get("HOME") == "/home",
           "the outer dropped the genuine environment hooks")
    expect(harness.sequence[1] == "stage" and
           harness.sequence[2:] == [f"case:{case['id']}" for case in CASES],
           "the inner ordering is not stage then J1..J12")
    expect(len(harness.written) == 1 and
           list(harness.written)[0].startswith(harness.fixture_evidence),
           "the receipt was not published into the guard's evidence directory")
    expect("--binary" not in harness.last_guarded_command,
           "the inner argv carries a staged binary the outer cannot have minted")
    expect(harness.used_designated == harness.fixture_designated
           and harness.used_designated != harness.saved_designated,
           "the run's guard layout did not live inside the harness's own "
           "private fixture")

    # Guard argv contract: the unchanged qualified supervisor, its limits, the
    # verified root parent, and the parseable inner tail.
    command = harness.last_guarded_command
    expect(command[:1] == [sys.executable] and "bounded-test.py" in command[1],
           "the guarded child is not the pinned supervisor")
    expect("--limit" in command and str(LIMIT_SECONDS) in command,
           "the guarded child does not carry the supervisor's limits")
    expect("--root-parent" in command and
           command[command.index("--root-parent") + 1] == harness.used_designated,
           "the guarded child does not adopt the run's designated scratch")
    tail = command[command.index("--") + 1:]
    expect(tail[:2] == ["python3", SCRIPT_PATH], "the inner tail is not this script")
    try:
        parsed = argument_parser().parse_args(tail[2:])
        expect(parsed.inner == "counted", "the inner argv does not parse the mode")
    except SystemExit:
        problems.append("the inner argv does not parse")

    # Supervisor pin mismatch: refusal, no launch.
    harness = control_harness()
    harness.supervisor_digest_value = "b" * 64
    exit_code, receipt = run_outer(harness)
    expect(exit_code == 125, "a supervisor pin mismatch did not refuse with 125")
    expect(harness.guarded_launches == 0, "a pin mismatch still launched the guard")

    # Route mismatch: the designated scratch's volume is not the verified Stash.
    harness = control_harness()
    harness.route_volume_uuid = "0" * 8 + ROUTE_UUID[8:]
    exit_code, receipt = run_outer(harness)
    expect(exit_code == 125, "a route mismatch did not refuse with 125")
    expect(harness.guarded_launches == 0, "a route mismatch still launched the guard")

    # Inner binding refusals through the REAL reader over the REAL fixture:
    # each variation materializes the exact real layout the pinned supervisor
    # would refuse, and the inner refuses before any stage or case effect.
    harness = control_harness()
    shutil.rmtree(harness.fixture_scratch)
    exit_code, _ignored_receipt = run_outer(harness)
    expect(isinstance(exit_code, int) and exit_code != 0,
           "an absent scratch did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("qualified_inner") is not True,
           "an absent scratch still qualified the inner")
    expect(harness.sequence.count("stage") == 0, "an absent scratch still staged")

    harness = control_harness()
    os.chmod(harness.fixture_scratch, 0o775)
    exit_code, _ignored_receipt = run_outer(harness)
    expect(isinstance(exit_code, int) and exit_code != 0,
           "a group-writable scratch did not fail the run")
    expect(harness.sequence.count("stage") == 0, "a group-writable scratch still staged")

    harness = control_harness()
    alias = os.path.join(harness.fixture_guard_root, "scratch-alias")
    os.symlink(harness.fixture_scratch, alias)
    harness.scratch_override = alias
    exit_code, _ignored_receipt = run_outer(harness)
    expect(isinstance(exit_code, int) and exit_code != 0,
           "a symlinked scratch did not fail the run")
    inner = harness.inner_receipt or {}
    expect(any("symlink" in refusal for refusal in inner.get("refused", [])),
           "the symlinked scratch was not refused as a symlink")

    harness = control_harness()
    os.chmod(harness.fixture_guard_root, 0o755)
    exit_code, _ignored_receipt = run_outer(harness)
    expect(exit_code != 0, "a non-private owned root did not fail the run")
    inner = harness.inner_receipt or {}
    expect(any("owned root" in refusal for refusal in inner.get("refused", [])),
           "the owned-root refusal was not named")

    harness = control_harness()
    elsewhere = os.path.join(harness.fixture_root, "elsewhere-evidence")
    os.makedirs(elsewhere)
    harness.evidence_override = elsewhere
    exit_code, _ignored_receipt = run_outer(harness)
    expect(isinstance(exit_code, int) and exit_code != 0,
           "evidence outside the owned root did not fail the run")
    inner = harness.inner_receipt or {}
    expect(any("outside the guard's owned root" in refusal
               for refusal in inner.get("refused", [])),
           "the outside evidence was not refused by containment")

    harness = control_harness()
    nested = os.path.join(harness.fixture_designated, "nested", "deeper",
                          "scratch")
    os.makedirs(nested)
    harness.scratch_override = nested
    exit_code, _ignored_receipt = run_outer(harness)
    expect(exit_code != 0, "an off-parent owned root did not fail the run")

    # A foreign scratch volume, through the REAL reader: only the volume
    # query's mount is pointed elsewhere (the external kernel/route query),
    # and the reader's fact then refuses the foreign uuid.
    harness = control_harness()
    saved_mount = base.SCRATCH_VOLUME_MOUNT
    base.SCRATCH_VOLUME_MOUNT = "/Volumes/elsewhere"
    try:
        exit_code, _ignored_receipt = run_outer(harness)
    finally:
        base.SCRATCH_VOLUME_MOUNT = saved_mount
    expect(isinstance(exit_code, int) and exit_code != 0,
           "a foreign scratch volume did not fail the run")

    # Foreign ownership cannot be materialized under this account, so the
    # pure containment check is driven directly over the reference fact
    # shape for the two uid variations (the real reader's uid fidelity is
    # the real-reader control's, over real files).
    def reference_facts():
        def directory_facts(path, mode, uid, islink=False):
            return {"path": path, "realpath": path, "present": True,
                    "isdir": not islink, "islink": islink, "uid": uid,
                    "mode": mode}
        return {
            "uid": os.getuid(),
            "volume_uuid": ROUTE_UUID,
            "path": "guarded/scratch",
            "root_parent": "guarded",
            "scratch_facts": directory_facts("guarded/scratch", 0o755, os.getuid()),
            "evidence_facts": directory_facts("guarded/evidence", 0o755, os.getuid()),
            "root_facts": directory_facts("guarded", 0o700, os.getuid()),
        }

    foreign_uid = os.getuid() + 1
    for name, mutate in (
            ("foreign scratch owner",
             lambda facts: facts["scratch_facts"].update(uid=foreign_uid)),
            ("foreign owned-root owner",
             lambda facts: facts["root_facts"].update(uid=foreign_uid))):
        facts = reference_facts()
        mutate(facts)
        uid_problems = guard_containment_problems(facts)
        expect(uid_problems,
               f"the containment check missed the {name}")

    # Wall deadline handoff: missing, malformed, stale, huge-but-expired.
    harness = control_harness()
    harness.strip_wall_deadline = True
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a missing wall deadline did not fail the run")
    inner = harness.inner_receipt or {}
    expect(any("WALL_DEADLINE" in refusal for refusal in inner.get("refused", [])),
           "a missing wall deadline was not refused by name")

    harness = control_harness()
    harness.wall_deadline_override = "not-a-number"
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a malformed wall deadline did not fail the run")

    harness = control_harness()
    harness.wall_deadline_override = str(10 ** 18)
    harness.monotonic_ns_value = 2 * 10 ** 18
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a numerically huge expired wall did not fail the run")
    inner = harness.inner_receipt or {}
    expect(all_cases_unexecuted(inner),
           "an expired wall did not record every case unexecuted")

    harness = control_harness()
    harness.wall_deadline_override = str(harness.monotonic_ns_value - 1)
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a stale wall deadline did not fail the run")
    inner = harness.inner_receipt or {}
    expect(all_cases_unexecuted(inner),
           "a stale wall deadline did not record every case unexecuted")

    # Clock advancing during the stage: the cases start past the closure
    # margin and are recorded unexecuted, honestly.
    harness = control_harness()
    harness.wall_deadline_override = str(harness.monotonic_ns_value + 100 * 10 ** 9)
    original_stage = harness.run_process

    def stage_advancing_clock(command, env, timeout):
        result = original_stage(command, env, timeout)
        if command[:2] == ["oxide", "build"]:
            harness.monotonic_ns_value = harness.monotonic_ns_value + 200 * 10 ** 9
        return result
    harness.run_process = stage_advancing_clock
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a stage that consumed the wall did not fail the run")
    inner = harness.inner_receipt or {}
    expect(all_cases_unexecuted(inner),
           "a consumed wall did not record every case unexecuted")

    # Clock advancing between cases: case J6 latches nothing (clean so far),
    # but once the clock crosses the cutoff the later cases are unexecuted.
    harness = control_harness()
    harness.wall_deadline_override = str(harness.monotonic_ns_value + 100 * 10 ** 9)
    original_case = harness.run_process

    def case_advancing_clock(command, env, timeout):
        result = original_case(command, env, timeout)
        if command[-1] == "J5":
            harness.monotonic_ns_value = harness.monotonic_ns_value + 200 * 10 ** 9
        return result
    harness.run_process = case_advancing_clock
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a consumed mid-run wall did not fail the counted run")
    inner = harness.inner_receipt or {}
    statuses = {c.get("case"): c.get("status") for c in inner.get("cases", [])}
    expect(statuses.get("J5") == "passed", "J5 should have passed before the cutoff")
    expect(all(statuses.get(case["id"]) == "unexecuted" for case in CASES[5:]),
           "the cases after the cutoff were not recorded unexecuted")

    # Cutoff just before a case: J1 passes, then the cutoff lands before J2.
    harness = control_harness()
    harness.wall_deadline_override = str(harness.monotonic_ns_value + 31 * 10 ** 9)
    original_case = harness.run_process

    def case_consuming_clock(command, env, timeout):
        result = original_case(command, env, timeout)
        if command[:2] != ["oxide", "build"]:
            harness.monotonic_ns_value = harness.monotonic_ns_value + 5 * 10 ** 9
        return result
    harness.run_process = case_consuming_clock
    exit_code, receipt = run_outer(harness)
    inner = harness.inner_receipt or {}
    statuses = {c.get("case"): c.get("status") for c in inner.get("cases", [])}
    expect(statuses.get("J1") == "passed", "J1 should have run inside the margin")
    expect(statuses.get("J2") == "unexecuted",
           "J2 should have been refused at the closure margin")

    # Drift: the store digest changes between the pre-stage capture and the
    # manifest mint, and the entry faults as drift before any case passes.
    harness = control_harness()
    harness.identity_drift_after_capture = True
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "an identity drift did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("fault", {}).get("kind") == "drift",
           "the identity drift was not recorded as the guarded fault")
    expect(not any(c.get("status") == "passed" for c in inner.get("cases", [])),
           "a drifted run still passed cases")
    expect(inner.get("pre_stage_identity", {}).get("store_source_sha256")
           != (inner.get("manifest") or {}).get("store_source_sha256"),
           "the receipt does not carry the drifted pre-stage identity")

    # Replaced staged binary: the pre-first-case re-hash disagrees with the
    # manifest's digest, and the entry faults as drift.
    harness = control_harness()
    harness.replaced_binary_digest = "c" * 64
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a replaced staged binary did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("fault", {}).get("kind") == "drift",
           "the replaced binary was not recorded as drift")

    # Stage outside the owned scratch: a case directory that escapes the run
    # root is a custody fault, and the run never qualifies.
    harness = control_harness()
    harness.case_directory_parent = "/elsewhere"
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "an escaped case directory did not fail the run")
    inner = harness.inner_receipt or {}
    expect(any(c.get("kind") == "custody" for c in inner.get("cases", [])),
           "the escaped case directory was not recorded as custody")

    # Stage refusal inside the guard: bounded failure evidence, never qualified.
    harness = control_harness()
    harness.stage_fault = ("stage", "the injected stage refusal")
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a stage refusal did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("fault", {}).get("kind") == "stage",
           "the stage refusal was not recorded as the guarded fault")
    expect(not any(c.get("status") == "passed" for c in inner.get("cases", [])),
           "a stage-refused run still passed cases")

    # Guard not closed: the inner receipt is clean but the guarded child
    # exits nonzero — the outer composes the honest unqualified receipt, and
    # the inner outcome alone never qualifies.
    harness = control_harness()
    harness.guard_exit_override = 1
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a nonzero guarded exit produced a nonzero-qualified run")
    inner = harness.inner_receipt or {}
    expect(inner.get("qualified_inner") is True,
           "the clean inner outcome was not preserved in the receipt")

    # Case fault: a failing case latches the counted run and the later cases
    # are skipped, honestly.
    harness = control_harness()
    harness.case_results = {"J1": types.SimpleNamespace(
        returncode=1,
        stdout=json.dumps({"case": "J1", "status": "failed",
                           "detail": "injected: refused"}) + "\n",
        stderr="")}
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a failing case did not fail the counted run")
    inner = harness.inner_receipt or {}
    statuses = {c.get("case"): c.get("status") for c in inner.get("cases", [])}
    expect(statuses.get("J1") == "failed", "the failing case was not recorded")
    expect(statuses.get("J12") == "skipped", "the latch did not skip the rest")

    # Bad child reply and duplicate/unknown verdicts refuse.
    harness = control_harness()
    harness.case_results = {"J1": types.SimpleNamespace(returncode=0, stdout="{}\n",
                                                        stderr="")}
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a bad child reply did not fail the run")
    harness = control_harness()
    duplicate = json.dumps({"case": "J1", "status": "passed", "detail": "x"})
    harness.case_results = {"J1": types.SimpleNamespace(
        returncode=0, stdout=duplicate + "\n" + duplicate, stderr="")}
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a duplicate verdict did not fail the run")
    harness = control_harness()
    harness.case_results = {"J1": types.SimpleNamespace(
        returncode=0,
        stdout=json.dumps({"case": "J99", "status": "passed", "detail": "x"}) + "\n",
        stderr="")}
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "an unknown-case verdict did not fail the run")

    # Stage executor exceptions: a remaining-capped stage timeout and a
    # failed stage spawn both become bounded stage faults — the inner
    # publishes its receipt and never crashes without evidence.
    harness = control_harness()
    runner_through_stage = harness.run_process

    def stage_timeout_expired(command, env, timeout):
        if command[:2] == ["oxide", "build"]:
            raise subprocess.TimeoutExpired(command, timeout)
        return runner_through_stage(command, env, timeout)
    harness.run_process = stage_timeout_expired
    exit_code, _ignored_receipt = run_outer(harness)
    expect(exit_code != 0, "a stage timeout did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("fault", {}).get("kind") == "stage",
           "the stage timeout was not recorded as a bounded stage fault")
    expect("receipt_path" in inner,
           "the stage timeout published no bounded receipt")
    expect(all_cases_unexecuted(inner),
           "a stage-timeout run did not record every case unexecuted")

    harness = control_harness()
    runner_through_spawn = harness.run_process

    def stage_spawn_oserror(command, env, timeout):
        if command[:2] == ["oxide", "build"]:
            raise OSError("the injected stage spawn failure")
        return runner_through_spawn(command, env, timeout)
    harness.run_process = stage_spawn_oserror
    exit_code, _ignored_receipt = run_outer(harness)
    expect(exit_code != 0, "a stage spawn failure did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("fault", {}).get("kind") == "stage",
           "the stage spawn failure was not recorded as a bounded stage fault")
    expect("receipt_path" in inner,
           "the stage spawn failure published no bounded receipt")
    expect(all_cases_unexecuted(inner),
           "a stage-spawn-failure run did not record every case unexecuted")

    # Pre-stage source failures are bounded too: a Fault and an OSError from
    # the identity seam (the real git query refusing or failing) land as a
    # bounded source fault whose receipt still publishes, with all twelve
    # groups unexecuted, no manifest, and no fabricated pre-stage binding.
    for name, injected in (
            ("git fault", Fault("source", "git rev-parse HEAD failed: injected")),
            ("git oserror", OSError("the injected git spawn failure"))):
        harness = control_harness()

        def failing_git(arguments, _error=injected):
            raise _error
        harness.git = failing_git
        exit_code, _ignored_receipt = run_outer(harness)
        expect(exit_code != 0,
               f"a pre-stage source failure ({name}) did not fail the run")
        inner = harness.inner_receipt or {}
        fault = inner.get("fault") or {}
        expect(fault.get("kind") == "source",
               f"the pre-stage source failure ({name}) was not a bounded source fault")
        expect("receipt_path" in inner,
               f"the pre-stage source failure ({name}) published no bounded receipt")
        expect(all_cases_unexecuted(inner),
               f"the pre-stage source failure ({name}) did not record every "
               f"case unexecuted")
        expect(inner.get("manifest") is None,
               f"the pre-stage source failure ({name}) fabricated a manifest")
        expect(inner.get("pre_stage_identity") is None,
               f"the pre-stage source failure ({name}) fabricated a pre-stage binding")

    # The REAL factory and the REAL stage adapter, end to end: the real
    # `InnerEffects.from_environment()` factory (never a harness-built
    # effects object) drives the actual inner composition, with only the
    # external subprocess runner (supplying raw Cargo artifact JSON, never a
    # preconstructed stage record), Git, the clock and the storage scripted.
    # The stage artifact names a fresh private fixture binary-as-data inside
    # this harness's own fixture that is hashed but never executed, so the
    # real artifact parser, feature validation, digest and re-hash all run
    # for real — and every write lands inside the fixture.
    harness = control_harness()
    harness.env[base.WALL_DEADLINE_ENV] = str(
        harness.monotonic_ns_value + 10_000 * 10 ** 9)
    try:
        with harness.real_factory():
            effects = InnerEffects.from_environment()
            run_root = os.path.join(harness.fixture_root, "stage-run")
            os.makedirs(run_root)
            staged = os.path.join(run_root, "pool", DRIVER_NAME)
            os.makedirs(os.path.dirname(staged))
            with open(staged, "wb") as handle:
                handle.write(b"\xcf\xfa\xed\xfe journal-proof binary-as-data"
                             b" (hashed, never executed)")
            with open(staged, "rb") as handle:
                fixture_digest = hashlib.sha256(handle.read()).hexdigest()
            harness.stage_output = (json.dumps({
                "reason": "compiler-artifact",
                "target": {"kind": ["example"], "crate_types": ["bin"],
                           "name": DRIVER_NAME, "edition": "2021"},
                "features": [PROOF_FEATURE],
                "executable": staged}) + "\n")
            receipt = run_cases_inner("counted", run_root, effects,
                                      int(harness.env[base.WALL_DEADLINE_ENV]))
            expect(receipt.get("qualified_inner") is True,
                   "the real-factory composition did not qualify")
            expect(len(receipt.get("cases", [])) == len(CASES)
                   and all(c.get("status") == "passed" for c in receipt["cases"]),
                   "the real-factory composition did not pass all twelve groups")
            expect((receipt.get("manifest") or {}).get("binary_sha256")
                   == fixture_digest,
                   "the manifest does not carry the real staged binary's digest")
            expect(all(receipt.get("pre_stage_identity", {}).get(key)
                       == (receipt.get("manifest") or {}).get(key)
                       for key in ("store_source_sha256", "driver_source_sha256")),
                   "the pre-stage and post-stage identities disagree on a clean run")
            build = harness.build_calls[0]
            expect(build["command"] == STAGE_COMMAND,
                   "the real adapter did not run the exact managed build command")
            expect(build["env"].get("RUSTUP_TOOLCHAIN") == TOOLCHAIN_CHANNEL
                   and build["env"].get("CARGO_NET_OFFLINE") == "true"
                   and build["env"].get("TMPDIR")
                   == os.path.join(run_root, "stage-tmp"),
                   "the real adapter did not apply the stage env scrub")
            expect(build["timeout"] == STAGE_BASE_TIMEOUT,
                   "the stage timeout was not the remaining-capped base")
            expect(harness.sequence[0] == "stage",
                   "the stage did not precede the case children in the sequence")
            expect(harness.stage_timeouts == [STAGE_BASE_TIMEOUT],
                   "the stage child ran with a wrong timeout")
            expect(harness.case_timeouts == [CASE_BASE_TIMEOUT] * len(CASES),
                   "the case children did not run at the remaining-capped base")
            expect(harness.digest_calls.count(staged) == 2,
                   "the staged binary was not hashed at stage time and re-hashed "
                   "before the first case")

            # The retired keyword/return contract must fail against the real
            # factory: no `run_stage_child` executor with the old `(env=,
            # run_root=, timeout=)` stage-record shape can resolve, and a
            # CompletedProcess can never stand in for the stage record.
            try:
                effects.run_stage_child(env=effects.env, run_root="ignored",
                                        timeout=STAGE_BASE_TIMEOUT)
                expect(False, "the retired stage executor keyword contract still resolved")
            except AttributeError:
                expect(True, "")

            # The factory's seam wiring, behaviorally: identity produces the
            # source-binding shape, the digest hashes real bytes, directory
            # creation lands under its parent, and the process seam receives
            # (command, env, timeout) positionally. A mis-wired factory —
            # swapped identity/digest positions, a reordered constructor —
            # fails here instead of crashing a real receipt-less inner.
            identity = effects.identity_fn()
            expect(isinstance(identity, dict)
                   and {"head", "tree", "store_source_sha256",
                        "driver_source_sha256"} <= set(identity),
                   "the factory's identity seam does not produce the source binding")
            probe_path = os.path.join(harness.fixture_root, "wiring-probe")
            with open(probe_path, "wb") as handle:
                handle.write(b"wiring probe bytes")
            expect(effects.digest_fn(probe_path)
                   == hashlib.sha256(b"wiring probe bytes").hexdigest(),
                   "the factory's digest seam does not hash real bytes")
            wiring_parent = os.path.join(harness.fixture_root, "wiring-parent")
            os.makedirs(wiring_parent, exist_ok=True)
            made_directory = effects.make_directory(wiring_parent, "wiring-child-")
            expect(os.path.isdir(made_directory)
                   and made_directory.startswith(wiring_parent + os.sep),
                   "the factory's directory seam does not create under its parent")
            effects.run_process(["oxide", "build"], {"WIRING": "1"}, 5.0)
            wiring_call = harness.build_calls[-1]
            expect(wiring_call["command"] == ["oxide", "build"]
                   and wiring_call["env"] == {"WIRING": "1"}
                   and wiring_call["timeout"] == 5.0,
                   "the factory's process seam is not (command, env, timeout)")
    finally:
        harness.cleanup()

    # The real stage adapter's failure matrix through the same real factory:
    # nonzero build, timeout, spawn OSError, and wrong/missing/duplicate/
    # unfeatured artifacts all fault the adapter as bounded stage faults.
    harness = control_harness()
    try:
        with harness.real_factory():
            effects_stage = InnerEffects.from_environment()
            fault_root = os.path.join(harness.fixture_root, "adapter-faults")
            os.makedirs(fault_root, exist_ok=True)

            def stage_fault(expected_detail):
                try:
                    effects_stage.stage(env=effects_stage.env, run_root=fault_root,
                                        timeout=min(STAGE_BASE_TIMEOUT, 90.0))
                    expect(False, expected_detail + " did not fault the real adapter")
                except Fault as error:
                    expect(error.kind == "stage",
                           expected_detail + " was not a bounded stage fault")

            def artifact_line(**overrides):
                document = {"reason": "compiler-artifact",
                            "target": {"kind": ["example"], "crate_types": ["bin"],
                                       "name": DRIVER_NAME, "edition": "2021"},
                            "features": [PROOF_FEATURE],
                            "executable": "/stage/" + DRIVER_NAME}
                document.update(overrides)
                return json.dumps(document) + "\n"

            harness.build_returncode = 101
            stage_fault("a failed stage build")
            harness.build_returncode = 0

            harness.stage_raise = subprocess.TimeoutExpired(STAGE_COMMAND, 90.0)
            stage_fault("a stage build timeout")
            harness.stage_raise = OSError("the adapter spawn refusal")
            stage_fault("a stage spawn OSError")
            harness.stage_raise = None

            for label, output in (
                    ("a missing journal_proof artifact", ""),
                    ("a bin-kind artifact (wrong target kind)",
                     artifact_line(target={"kind": ["bin"], "name": DRIVER_NAME,
                                           "crate_types": ["bin"],
                                           "edition": "2021"})),
                    ("duplicate journal_proof artifacts",
                     artifact_line() + artifact_line()),
                    ("an artifact without the proof feature",
                     artifact_line(features=["default"]))):
                harness.stage_output = output
                stage_fault(label)
            harness.stage_output = artifact_line()
    finally:
        harness.cleanup()

    # An unexpected adapter exception (outside Fault/TimeoutExpired/OSError)
    # must still land as a bounded first/secondary infrastructure fault with
    # all twelve groups unexecuted and the available pre-stage binding —
    # never an unhandled TypeError with no receipt and no fabricated manifest.
    harness = control_harness()
    runner_through_type_error = harness.run_process

    def stage_type_error(command, env, timeout):
        if command[:2] == ["oxide", "build"]:
            raise TypeError(
                "run_stage_child() got an unexpected keyword argument 'run_root'")
        return runner_through_type_error(command, env, timeout)
    harness.run_process = stage_type_error
    exit_code, _ignored_receipt = run_outer(harness)
    expect(exit_code != 0, "an unexpected adapter exception did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("fault", {}).get("kind") == "stage",
           "the unexpected adapter exception was not a bounded stage fault")
    expect("receipt_path" in inner,
           "the unexpected adapter exception published no bounded receipt")
    expect(all_cases_unexecuted(inner),
           "an adapter-exception run still executed case groups")
    expect(bool(inner.get("pre_stage_identity")),
           "the adapter-exception receipt carries no pre-stage binding")
    expect(not (inner.get("manifest") or {}).get("binary_sha256"),
           "a failed stage still fabricated a manifest binary digest")

    # Fixture isolation under the platform's write audit: one full counted
    # composition through the real factories writes only inside this run's
    # own fixture — the shared designated scratch root (and everything else)
    # sees nothing, which is exactly the regression the audit-hook
    # reproduction caught two heads ago.
    #
    # The capture is scoped so only writes made by the audited call are
    # judged: garbage-collected fixture teardown from other controls is
    # neither in the window nor misclassified. Pending collections are
    # flushed before the window, collection is suspended for the window
    # (restored after), the audited call's own harness fixture stays alive
    # for the whole capture, and the harness is closed deterministically
    # outside the window — never by GC. Teardown events carry fd-relative
    # names that cannot be attributed to a location; such an event inside
    # the window is refused as unattributable rather than blanket-ignored.
    harness = control_harness()
    write_events = []

    def write_audit(event, args):
        if event == "os.mkdir":
            write_events.append(("os.mkdir", args[0]))
        elif event == "open":
            path, _mode, flags = args
            if isinstance(path, str) and (
                    flags & os.O_WRONLY or flags & os.O_RDWR
                    or flags & os.O_CREAT or flags & os.O_APPEND
                    or flags & os.O_TRUNC):
                write_events.append(("open", path))
        elif event in ("os.rename", "os.replace", "shutil.copyfile",
                       "os.remove", "os.rmdir", "os.truncate"):
            for value in args:
                if isinstance(value, str):
                    write_events.append((event, value))

    sys.addaudithook(write_audit)
    gc_was_enabled = gc.isenabled()
    gc.collect()
    gc.disable()
    try:
        with harness.real_factory():
            window_start = len(write_events)
            exit_code, _ignored_receipt = outer_half("counted",
                                                     OuterEffects.from_environment())
            window = write_events[window_start:]
    finally:
        if gc_was_enabled:
            gc.enable()
    harness.cleanup()
    expect(exit_code == 0, "the audited clean run did not pass")
    allowed_roots = (harness.fixture_root + os.sep,)
    stray = [entry for entry in window
             if os.path.isabs(entry[1])
             and not any(entry[1].startswith(root) or entry[1] == os.devnull
                         for root in allowed_roots)]
    expect(not stray,
           f"the audited composition wrote outside its own private fixture "
           f"(the shared designated scratch root included): {stray[:4]}")
    unattributable = [entry for entry in window if not os.path.isabs(entry[1])]
    expect(not unattributable,
           f"the audited window recorded fd-relative writes that cannot be "
           f"attributed to a location: {unattributable[:4]}")

    # The real reader: the containment checks must hold against the REAL
    # InnerEffects.guard_facts — where the symlink regression sat — on a tiny
    # real private fixture, with only the external route query, the clock and
    # the storage faked. No subprocess, no child, no stage, no guarded
    # launch. The fixture is one fresh per-run directory under this task's
    # private TMPDIR; the module's DESIGNATED_SCRATCH points at the fixture's
    # own realpath for the duration (restored after), so the reference
    # geometry — the owned root a direct child of the designated scratch —
    # holds for the fixture without touching the shared designated scratch.
    tmp = tempfile.TemporaryDirectory(prefix="journal-facts-")
    fixture_parent = os.path.realpath(tmp.name)
    saved_designated = DESIGNATED_SCRATCH
    parent_alias = None
    evidence_link = None
    globals()["DESIGNATED_SCRATCH"] = fixture_parent
    try:
        owned = os.path.join(fixture_parent, "owned-root")
        scratch = os.path.join(owned, "scratch")
        evidence = os.path.join(owned, "evidence")
        os.makedirs(scratch, mode=0o755)
        os.makedirs(evidence, mode=0o755)
        os.chmod(owned, 0o700)
        link = os.path.join(owned, "scratch-alias")
        os.symlink(scratch, link)
        evidence_link = os.path.join(owned, "evidence-alias")
        os.symlink(evidence, evidence_link)
        parent_alias = os.path.join(fixture_parent, "parent-alias")
        os.symlink(owned, parent_alias)
        outside = os.path.join(fixture_parent, "outside-evidence")
        os.makedirs(outside, mode=0o755)

        saved_route_query = base.scratch_route_facts
        try:
            base.scratch_route_facts = lambda path, mount: {
                "path": path, "volume_uuid": ROUTE_UUID,
                "owner_uid": os.getuid(), "uid": os.getuid(),
                "mode": 0o755, "present": True,
                "realpath": os.path.realpath(path),
            }
            real_effects = InnerEffects.from_environment()

            accepted = real_effects.guard_facts(scratch, evidence)
            expect(guard_containment_problems(accepted) == [],
                   "the real reader refused the actual 0755/0700 layout")

            symlinked = real_effects.guard_facts(link, evidence)
            symlink_problems = guard_containment_problems(symlinked)
            expect(any("symlink" in problem for problem in symlink_problems),
                   "the real reader accepted a symlinked scratch leaf")

            linked_evidence = real_effects.guard_facts(scratch, evidence_link)
            evidence_symlink_problems = guard_containment_problems(linked_evidence)
            expect(any("symlink" in problem
                       for problem in evidence_symlink_problems),
                   "the real reader accepted a symlinked evidence leaf")

            via_alias = os.path.join(parent_alias, "scratch")
            alias_facts = real_effects.guard_facts(via_alias, evidence)
            alias_problems = guard_containment_problems(alias_facts)
            expect(any("alias or a symlink" in problem
                       or "is a symlink" in problem
                       for problem in alias_problems),
                   "the real reader accepted a parent-alias route")

            outside_facts = real_effects.guard_facts(scratch, outside)
            outside_problems = guard_containment_problems(outside_facts)
            expect(any("outside the guard's owned root" in problem
                       for problem in outside_problems),
                   "the real reader accepted evidence outside the owned root")
        finally:
            base.scratch_route_facts = saved_route_query
    finally:
        # The designated scratch is restored before anything tears down, and
        # every cleanup name was initialised before the try: an early failure
        # removes only the symlinks this control created (if any) and closes
        # the fresh fixture, never raising past this point.
        globals()["DESIGNATED_SCRATCH"] = saved_designated
        for leftover in (parent_alias, evidence_link):
            if leftover:
                try:
                    os.remove(leftover)
                except OSError:
                    pass
        tmp.cleanup()

    # Report failure: an unwritable receipt unqualifies the inner.
    harness = control_harness()
    harness.fail_report_write = True
    exit_code, receipt = run_outer(harness)
    expect(exit_code != 0, "a report failure did not fail the run")
    inner = harness.inner_receipt or {}
    expect(inner.get("qualified_inner") is not True,
           "a failed receipt write still qualified the inner")

    # The guarded runner's stdout handoff: the real `run_inner` entry (the
    # exact function `--inner` invokes) returns one bounded JSON receipt line
    # beside its exit code — on a clean counted pass, on a guarded fault,
    # on a publication failure and in diagnostic mode — because the
    # unguarded outer parses exactly this channel.
    def handoff_harness():
        harness = control_harness()
        harness.env.update({
            "OXIDE_TASK_SCRATCH_ROOT": harness.fixture_scratch,
            "OXIDE_PROOF_EVIDENCE": harness.fixture_evidence,
            base.WALL_DEADLINE_ENV: str(harness.monotonic_ns_value + 300 * 10 ** 9),
        })
        return harness

    harness = handoff_harness()
    try:
        with harness.real_factory():
            code, line = run_inner(argument_parser().parse_args(["--inner", "counted"]))
        expect(code == 0, "a clean counted inner did not exit zero")
        expect("\n" not in line, "the receipt handoff is not one line")
        document = json.loads(line)
        expect(document.get("qualified_inner") is True,
               "the handed-off receipt does not qualify the clean pass")
        expect(document.get("stage") == "inner"
               and len(document.get("cases", [])) == len(CASES),
               "the handed-off receipt does not carry the twelve groups")
        expect(len(line) <= RECEIPT_LINE_BOUND, "the receipt line is not bounded")
        oversized = {"cases": [{"detail": "x" * (RECEIPT_LINE_BOUND + 1)}]}
        fallback = bounded_receipt_line(oversized)
        expect("\n" not in fallback and "bounded" in fallback,
               "the oversized receipt did not fall back to the bounded line")
    finally:
        harness.cleanup()

    harness = handoff_harness()
    harness.stage_fault = ("stage", "the injected handoff fault")
    try:
        with harness.real_factory():
            code, line = run_inner(argument_parser().parse_args(["--inner", "counted"]))
        expect(code == 1, "a faulted inner did not exit nonzero")
        document = json.loads(line)
        expect(document.get("fault", {}).get("kind") == "stage",
               "the faulted handoff does not carry the guarded fault")
        expect(all_cases_unexecuted(document),
               "the faulted handoff did not record every group unexecuted")
    finally:
        harness.cleanup()

    harness = handoff_harness()
    harness.fail_report_write = True
    try:
        with harness.real_factory():
            code, line = run_inner(argument_parser().parse_args(["--inner", "counted"]))
        expect(code == 1, "a publication failure did not exit nonzero")
        document = json.loads(line)
        expect(bool(document.get("receipt_write_fault")),
               "the publication failure is not recorded in the handed-off receipt")
        expect(document.get("qualified_inner") is not True,
               "a publication failure still qualified the inner")
    finally:
        harness.cleanup()

    harness = handoff_harness()
    try:
        with harness.real_factory():
            code, line = run_inner(argument_parser().parse_args(["--inner", "diagnostic"]))
        expect(code == 1, "the diagnostic inner did not stay nonzero")
        document = json.loads(line)
        expect(document.get("mode") == "diagnostic"
               and len(document.get("cases", [])) == len(CASES),
               "the diagnostic handoff does not carry its mode and groups")
    finally:
        harness.cleanup()

    # Diagnostic: keep-going records every case and never qualifies.
    harness = control_harness()
    harness.case_results = {"J3": types.SimpleNamespace(
        returncode=1,
        stdout=json.dumps({"case": "J3", "status": "failed",
                           "detail": "injected: refused"}) + "\n",
        stderr="")}
    exit_code, receipt = run_outer(harness, "diagnostic")
    expect(exit_code != 0, "the diagnostic did not stay nonzero")
    inner = harness.inner_receipt or {}
    expect(len([c for c in inner.get("cases", [])
                if c.get("status") in ("passed", "failed")]) == len(CASES),
           "the diagnostic did not record every case")

    # Retained evidence after simulated scratch cleanup: the receipt lives in
    # the guard's evidence directory, not the scratch that cleanup removes.
    harness = control_harness()
    run_outer(harness)
    receipt_paths = list(harness.written)
    expect(receipt_paths and all(path.startswith(harness.fixture_evidence)
                                 for path in receipt_paths),
           "the receipt is not inside the guard's evidence directory")
    harness.directories.clear()  # simulated guard scratch cleanup
    expect(all(path in harness.written for path in receipt_paths),
           "the retained evidence did not survive the simulated scratch cleanup")

    return problems

def case_table_problems():
    """Every group names a real driver method and real carried regressions."""
    problems = []
    with open(DRIVER_SOURCE, "r", encoding="utf-8") as handle:
        driver = handle.read()
    methods = set(re.findall(r'"(J\d+)" => \w+', driver))
    names = rust_test_names()
    for case in CASES:
        if case["method"] not in methods:
            problems.append(f"case {case['id']} names no driver method the example runs")
        for carried in case["carried_by"]:
            if carried not in names:
                problems.append(
                    f"case {case['id']} carries no named regression "
                    f"({carried} is not a test in crates/barge/src/operation.rs)"
                )
    ids = [case["id"] for case in CASES]
    if len(ids) > 12:
        problems.append(f"the case map holds {len(ids)} groups, over the twelve bound")
    if len(set(ids)) != len(ids):
        problems.append("the case map repeats an id")
    for token in FORBIDDEN_TOKENS:
        if token in source_text():
            problems.append(f"the proof script contains `{token}`")
    return problems


def source_text():
    with open(SCRIPT_PATH, "r", encoding="utf-8") as handle:
        return handle.read()


def rust_test_names():
    """The regression names this proof carries, read from the real source:
    only functions annotated `#[test]` count."""
    names = set()
    previous = ""
    with open(STORE_SOURCE, "r", encoding="utf-8") as handle:
        for line in handle:
            stripped = line.strip()
            if stripped.startswith("fn ") and "(" in stripped \
                    and "#[test]" in previous:
                names.add(stripped[3:].split("(")[0].strip())
            if stripped:
                previous = stripped
    return names


def feature_problems():
    """The driver target requires the proof feature; no default enables it."""
    problems = []
    with open(MANIFEST, encoding="utf-8") as handle:
        manifest = handle.read()
    if f'name = "{DRIVER_NAME}"' not in manifest:
        problems.append(f"the {DRIVER_NAME} example target is not declared")
    if f'required-features = ["{PROOF_FEATURE}"]' not in manifest:
        problems.append("the driver example does not require the proof feature")
    if f"{PROOF_FEATURE} = []" not in manifest:
        problems.append(f"the proof feature {PROOF_FEATURE} is not declared")
    if re.search(r"^default\s*=", manifest, re.MULTILINE):
        problems.append("a default feature is declared, which would enable the proof build by default")
    return problems


def declaration_problems():
    """The effigy declarations exist, are held, and the selectors are
    effect-free."""
    problems = []
    with open(EFFIGY, encoding="utf-8") as handle:
        effigy = handle.read()
    for selector in ("journal:native:test", "journal:native:plan"):
        if f'"{selector}"' not in effigy:
            problems.append(f"the effect-free selector {selector} is not declared")
    for task in ("journal:native", "journal:native:diagnostic"):
        if f'[tasks."{task}"]' not in effigy:
            problems.append(f"the held task {task} is not declared")
    if 'run = "python3 scripts/native/journal-proof.py counted"' not in effigy:
        problems.append("the counted task does not run this script's counted mode")
    if 'run = "python3 scripts/native/journal-proof.py diagnostic"' not in effigy:
        problems.append("the diagnostic task does not run this script's diagnostic mode")
    return problems


def supervisor_problems():
    """Why the pin and the installed supervisor's bytes disagree. Spawns
    nothing; an absent supervisor is reported, never fabricated as verified."""
    path = supervisor_path()
    try:
        found = base.sha256_file(path)
    except OSError as error:
        return [f"the pinned supervisor is unreadable: {error}"]
    if found != base.SUPERVISOR_SHA256:
        return [f"the supervisor at {path} is sha256 {found}, not the pinned {base.SUPERVISOR_SHA256}"]
    return []


def route_readiness_problems():
    """Why this host cannot verify the designated scratch route right now."""
    facts = base.scratch_route_facts()
    return designated_route_problems(facts)


def argv_shape_problems():
    """The outer guarded command must end in an inner argv this script's own
    parser accepts, binding the mode."""
    problems = []
    command = supervisor_command(inner_command("counted"))
    tail = command[command.index("--") + 1:]
    if tail[0] != "python3" or tail[1] != SCRIPT_PATH:
        problems.append("the guarded command does not run this script's inner runner")
    try:
        parsed = argument_parser().parse_args(tail[2:])
    except SystemExit:
        problems.append("the guarded inner argv does not parse")
        return problems
    if parsed.inner != "counted":
        problems.append("the guarded inner argv does not bind the mode")
    if "--limit" not in command or str(LIMIT_SECONDS) not in command:
        problems.append("the guarded command does not carry the supervisor's limits")
    if "--root-parent" in command and \
            command[command.index("--root-parent") + 1] != DESIGNATED_SCRATCH:
        problems.append("the supervisor does not adopt the designated Stash scratch")
    return problems


def execution_readiness():
    """Whether every executable prerequisite is present on this host. The
    plan reports `execution_ready` honestly: false with reasons while any
    prerequisite is absent, true only at complete source readiness — the
    entry composition controls are part of readiness, so a counted or
    diagnostic entry that cannot compose is never reported ready."""
    problems = []
    problems.extend(supervisor_problems())
    problems.extend(route_readiness_problems())
    problems.extend(case_table_problems())
    problems.extend(feature_problems())
    problems.extend(declaration_problems())
    problems.extend(argv_shape_problems())
    problems.extend(composition_controls())
    return problems


def plan_document():
    readiness = execution_readiness()
    return {
        "schema": PLAN_SCHEMA,
        "proof": "the operation journal's native cases, journal-only",
        "execution_ready": len(readiness) == 0,
        "execution_blockers": readiness,
        "composition": {
            "outer": "unguarded: verifies the supervisor pin and the designated "
                     "Stash route metadata, mints the wall deadline once, "
                     "launches one guarded child, and returns the actual guard "
                     "exit; it creates no fixture, stage root or receipt",
            "inner": "guarded: validates the guard-supplied layout with the "
                     "reference semantics (scratch and evidence canonical, "
                     "un-symlinked, owned by this account, free of group/other "
                     "write bits — the guard creates them 0755; the owned root "
                     "that parents them private 0700 and a direct child of the "
                     "verified scratch parent; the scratch volume the verified "
                     "Stash UUID) plus the wall deadline, creates the fixture "
                     "and stage roots inside the owned scratch, stages through "
                     "the managed Oxide build, mints the manifest and compares "
                     "it with the pre-stage identity, re-hashes the staged "
                     "binary, runs the cases, and publishes the receipt into "
                     "the guard evidence before any cleanup",
            "clock_domain": "time.monotonic_ns() in both halves; the wall "
                            "deadline is BARGE_PROOF_WALL_DEADLINE_NS, minted "
                            "once by the outer before the launch; the "
                            "supervisor's raw OXIDE_PROOF_DEADLINE_NS is never "
                            "mixed in; remaining is recomputed before the "
                            "stage, every case and publication, and every "
                            "executor timeout is capped by remaining minus "
                            "the 30 s closure margin",
        },
        "driver": {
            "example": DRIVER_NAME,
            "feature": PROOF_FEATURE,
            "stage_command": " ".join(STAGE_COMMAND),
        },
        "supervisor": {"sha256": base.SUPERVISOR_SHA256, "path": supervisor_path()},
        "route": {"scratch": DESIGNATED_SCRATCH, "uuid": ROUTE_UUID},
        "bounds": {"wall_seconds": LIMIT_SECONDS, "scratch_mib": SCRATCH_BUDGET_MIB,
                   "image": None, "work_stop_seconds": WORK_STOP_SECONDS,
                   "fit": "unmeasured"},
        "receipt": {"name": RECEIPT_NAME,
                    "inner_vs_guard_closure": "the receipt records the inner native "
                                              "outcome; the guard's group closure is the "
                                              "supervisor's separate fact"},
        "premises": [{"premise": p, "disposition": d} for p, d in PREMISES],
        "cases": CASES,
        "held": "counted and diagnostic are owner-activated; the effigy "
                "declarations activate nothing",
    }


def self_check():
    """The apparatus only: pins, case table, feature, declarations, and the
    composition controls through the actual entries with faked seams."""
    problems = []
    problems.extend(case_table_problems())
    problems.extend(feature_problems())
    problems.extend(declaration_problems())
    problems.extend(composition_controls())
    return problems


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic"])
    parser.add_argument("--inner", nargs="?", const="counted", default=None)
    return parser


def main(argv):
    parser = argument_parser()
    arguments = parser.parse_args(argv)
    if arguments.self_check:
        problems = self_check()
        if problems:
            for problem in problems:
                print(f"problem: {problem}", file=sys.stderr)
            return 1
        print(f"journal-proof apparatus: OK ({len(CASES)} case groups, "
              f"{sum(len(c['carried_by']) for c in CASES)} named regressions, "
              "entry composition controls passed through the real "
              "from_environment factories, no image, no guarded launch, no "
              "child process; the only storage is a set of fresh private "
              "per-run fixture directories under this run's own TMPDIR — "
              "one per harness, with each harness's real guard layout "
              "materialized inside its own fixture and closed "
              "deterministically — and no entry is ever created in the "
              "shared designated scratch root)")
        return 0
    if arguments.plan:
        print(json.dumps(plan_document(), indent=2))
        return 0
    if arguments.inner is not None:
        code, line = run_inner(arguments)
        print(line)
        return code
    if arguments.mode in ("counted", "diagnostic"):
        # The owner-activated entry: the unguarded outer half verifies the
        # pins and launches the one guarded child, returning the actual guard
        # exit. Never invoked by an ordinary selector.
        exit_code, receipt = outer_half(arguments.mode,
                                        OuterEffects.from_environment())
        print(json.dumps(receipt))
        return exit_code
    parser.error("name a mode: counted or diagnostic (or --inner, --self-check, --plan)")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
