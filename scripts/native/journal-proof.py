#!/usr/bin/env python3
"""Barge's operation-journal native proof: the single-authority store's cases
on fresh private filesystem fixtures, under the qualified shared supervisor.

  python3 scripts/native/journal-proof.py --self-check  # apparatus only, effect-free
  python3 scripts/native/journal-proof.py --plan        # the plan, effect-free
  python3 scripts/native/journal-proof.py counted       # the author pass (owner-activated)
  python3 scripts/native/journal-proof.py diagnostic    # the uncounted twin (owner-activated)

It is owner-activated. No ordinary Effigy selector runs a counted or diagnostic
pass, and the declarations in `effigy.toml` activate nothing.

What it is, in one sentence: after the pinned supervisor's bytes and the
verified private Stash scratch route are checked, and under the genuine guard
environment (OXIDE_TASK_SCRATCH_ROOT and OXIDE_PROOF_EVIDENCE, as the Effigy
and Nucleus admission supplies them), it stages the `proof-journal` feature
build of HEAD (`crates/barge/examples/journal_proof.rs`, required-features
gated) through the ordinary managed Oxide build, mints one source-bound
manifest (HEAD, tree, Cargo.lock, toolchain, feature, driver source and store
source digests, staged binary hash), and runs twelve journal-only case groups:
each case is one direct child of this script's inner runner, itself a child of
the unchanged qualified supervisor (`fcfda2b0…`), on one fresh private fixture
inside the guard's owned scratch — a private state directory, one synthetic
volume identity, no image.

Counted: every case must pass; the first fault latches and the remaining cases
are recorded as skipped or unexecuted, honestly; the receipt is
`qualified: true` only on a clean, fully verified pass. Diagnostic: keep going
within the safe custody and work budget, record every case, always write
`qualified: false`, and always exit nonzero — it proves nothing on its own.
Neither mode ever reports `qualified: true` with an unverified supervisor, a
missing or changed source or manifest, duplicate, missing or unknown cases, an
unconfirmed child custody, a fault, or an expired budget. The inner native
outcome this receipt records is one fact; the guard's own group closure is a
separate fact the supervisor owns, and this script never asserts it.

The driver labels every injected failure in its verdict evidence. No output
root is deleted, no image is attached, and no real account, drive or install
is touched. The bounds — 300 s wall, 512 MiB owned scratch, no image — remain
unactivated here; the fit is unmeasured until the owner-activated diagnostic.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import re
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
# The same envelope the qualified supervisor gives every elevated proof. The
# fit is unmeasured: a concrete bound impediment is reported, never enlarged.
LIMIT_SECONDS = base.LIMIT_SECONDS
SCRATCH_BUDGET_MIB = base.SCRATCH_BUDGET_MIB
WORK_STOP_SECONDS = 30
DESIGNATED_SCRATCH = base.DESIGNATED_SCRATCH
ROUTE_UUID = "8A660440-C919-45C6-A3F1-3365B9BD2006"
TOOLCHAIN_CHANNEL = base.TOOLCHAIN_CHANNEL
RUN_ROOT_PREFIX = "journal-proof-"
STAGE_COMMAND = [
    "oxide", "build", "-p", "barge", "--locked", "--offline",
    "--features", PROOF_FEATURE, "--example", DRIVER_NAME,
    "--message-format", "json",
]

# Each group names the driver case it runs and the real regressions that
# carry its claim in `crates/barge/src/operation.rs`. The self-check proves
# both directions against the actual source.
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

# The nine Oxide workflow premises, each with the actual disposition this
# journal-only proof carries or an explicit n/a.
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
     "inherited OXIDE, XDG, BARGE and Rust-flag variables"),
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
     "the inner runner creates its run directory inside the guard's owned "
     "scratch and nowhere else"),
    ("fresh mkdtemp fixture directories under the owned root",
     "every case group's fixture is one fresh mkdtemp directory under the "
     "owned run directory"),
    ("qualified supervisor pinned by SHA-256; no second supervisor",
     "the pinned digest below verifies the installed supervisor's bytes before "
     "any case runs"),
)


class Fault(Exception):
    """One recorded fault; counted runs latch it, diagnostics keep going."""

    def __init__(self, kind, detail):
        super().__init__(detail)
        self.kind = kind
        self.detail = detail


class Contract:
    """The occurrence's external effects, as callables.

    A real pass builds one from the platform (`from_environment`); the
    effect-free controls build one whose executors, clock, identity and
    storage are fakes with scripted answers, so the actual entry composition
    runs with only the external seams substituted.
    """

    def __init__(self, env, run_child, supervisor_digest, route_facts, git,
                 sha256_file, read_file, monotonic, make_directory, write_file):
        self.env = env
        self.run_child = run_child
        self.supervisor_digest = supervisor_digest
        self.route_facts = route_facts
        self.git = git
        self.sha256_file = sha256_file
        self.read_file = read_file
        self.monotonic = monotonic
        self.make_directory = make_directory
        self.write_file = write_file

    @staticmethod
    def from_environment():
        """The real occurrence: platform executors, clock, git and files."""
        environment = dict(os.environ)

        def run_child(command, env, timeout):
            return subprocess.run(command, env=env, capture_output=True, text=True,
                                  timeout=timeout)

        def git(arguments):
            result = subprocess.run(["git", *arguments], cwd=REPO_ROOT,
                                    capture_output=True, text=True, timeout=60)
            if result.returncode != 0:
                raise Fault("source",
                            f"git {' '.join(arguments)} failed: {result.stderr[-200:]}")
            return result.stdout.strip()

        def sha256_file(path):
            digest = hashlib.sha256()
            with open(path, "rb") as handle:
                for chunk in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(chunk)
            return digest.hexdigest()

        def read_file(path):
            with open(path, "r", encoding="utf-8") as handle:
                return handle.read()

        def make_directory(parent, prefix):
            return tempfile.mkdtemp(prefix=prefix, dir=parent)

        def write_file(path, text):
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(text)

        def route_facts(path):
            facts = base.scratch_route_facts(
                os.path.realpath(path), base.SCRATCH_VOLUME_MOUNT)
            designated = os.path.realpath(DESIGNATED_SCRATCH)
            real = facts.get("realpath", "")
            facts["within_designated"] = (
                real == designated or real.startswith(designated + os.sep))
            return facts

        return Contract(environment, run_child,
                        lambda: base.sha256_file(base.supervisor_path()),
                        route_facts,
                        git, sha256_file, read_file,
                        time.monotonic, make_directory, write_file)


def supervisor_path():
    return base.supervisor_path()


def verified_scratch_root(contract):
    """The guard's own scratch root, verified against the pinned Stash route.

    Returns the canonical directory the supervisor must adopt and the inner
    must build in, or a Fault. The guard's OXIDE_TASK_SCRATCH_ROOT is the
    genuine Effigy/Nucleus admission hook; it is accepted only when it really
    resolves onto the verified private Stash volume inside the designated
    barge scratch — never on faith, never by naming another root.
    """
    root = contract.env.get("OXIDE_TASK_SCRATCH_ROOT")
    if not root:
        raise Fault("guard", "OXIDE_TASK_SCRATCH_ROOT is absent; no verified scratch")
    facts = contract.route_facts(root)
    problems = route_problems(facts)
    if problems:
        raise Fault("route", "; ".join(problems))
    return facts["realpath"]


def source_identity(contract):
    """Bind the actual source the staged build came from."""
    return {
        "head": contract.git(["rev-parse", "HEAD"]),
        "tree": contract.git(["rev-parse", "HEAD^{tree}"]),
        "cargo_lock_sha256": contract.sha256_file(CARGO_LOCK),
        "toolchain_sha256": contract.sha256_file(TOOLCHAIN_FILE),
        "toolchain_channel": TOOLCHAIN_CHANNEL,
        "features": [PROOF_FEATURE],
        "stage_command": " ".join(STAGE_COMMAND),
        "driver_source_sha256": contract.sha256_file(DRIVER_SOURCE),
        "store_source_sha256": contract.sha256_file(STORE_SOURCE),
    }


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


def inner_command(mode, run_root):
    """The guarded inner argv: this script's inner runner, bound to the mode
    and this run's verified owned root. The inner stages and binds the
    source itself, inside the guard."""
    return [
        "python3", SCRIPT_PATH, "--inner", mode,
        "--run-root", run_root,
    ]


def supervisor_command(inner_argv, root_parent):
    """The unchanged qualified supervisor wrapping the inner argv: the one
    guarded child a native run spawns, adopting exactly the verified root
    the inner builds in."""
    return [
        sys.executable, base.supervisor_path(),
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", root_parent,
        "--", *inner_argv,
    ]


def route_problems(facts):
    """Why the scratch root is not the verified private Stash route."""
    problems = []
    if not facts.get("present"):
        problems.append(f"the scratch root {facts.get('path')} is absent")
    if facts.get("volume_uuid") != ROUTE_UUID:
        problems.append(
            f"the scratch root's volume is {facts.get('volume_uuid')}, "
            f"not the verified private Stash {ROUTE_UUID}"
        )
    if facts.get("within_designated") is not True:
        problems.append(
            f"the scratch root {facts.get('path')} is not inside the designated "
            f"barge scratch {DESIGNATED_SCRATCH}"
        )
    if facts.get("owner_uid") is not None and facts.get("uid") is not None \
            and facts.get("owner_uid") != facts.get("uid"):
        problems.append("the scratch root is not owned by this account")
    mode = facts.get("mode")
    if isinstance(mode, int) and mode & 0o077:
        problems.append(f"the scratch root mode {mode!r} is not owner-only")
    return problems


def inner_deadline(env, monotonic):
    """The supervisor's deadline, as the guarded inner inherits it; the
    supervisor's own hook is the truth, and the wall seconds are the fallback
    an unbudgeted control run takes."""
    raw = env.get("OXIDE_PROOF_DEADLINE_NS")
    if raw:
        try:
            deadline_ns = int(raw)
            now_ns = monotonic() * 1_000_000_000
            return monotonic() + max(0.0, (deadline_ns - now_ns) / 1_000_000_000)
        except ValueError:
            pass
    return monotonic() + LIMIT_SECONDS


SOURCE_IDENTITY_FIELDS = ("head", "tree", "cargo_lock_sha256", "toolchain_sha256",
                          "driver_source_sha256", "store_source_sha256")


def identity_drift(first, second):
    """The source fields two identities must agree on, and where they do not."""
    drifts = [field for field in SOURCE_IDENTITY_FIELDS
              if first.get(field) != second.get(field)]
    return drifts


def run_cases_inner(mode, run_root, env, deadline, make_directory, stage_fn,
                    case_fn, identity_fn, digest_fn):
    """The inner composition: capture the pre-stage identity, stage the proof
    build inside the guard, mint the source-bound manifest here and require
    it to equal the pre-stage identity (drift refuses), re-hash the staged
    binary before the first case, and run the twelve fixed case groups, each
    one direct driver child in its own fresh fixture directory under the
    owned run root — counted fail-fast or diagnostic keep-going — and return
    one inner receipt.

    This is the function the `--inner` runner calls with real child
    execution, and the effect-free controls call with only the stage and
    case executors, the source identity and the digest faked — the
    composition itself, including the in-guard stage, the manifest mint and
    the drift comparison, is never a fake.
    """
    pre_stage = identity_fn()
    stage = stage_fn(env, run_root)
    manifest = identity_fn()
    drifts = identity_drift(pre_stage, manifest)
    if drifts:
        raise Fault("drift",
                    f"the checkout changed during the in-guard stage: {', '.join(drifts)}")
    manifest["binary_sha256"] = stage["binary_sha256"]
    # The staged pool binary is re-hashed before the first case: a replaced
    # binary is drift, never the manifest's evidence.
    if digest_fn(stage["binary"]) != stage["binary_sha256"]:
        raise Fault("drift", "the staged binary changed after the stage")
    incomplete = manifest_complete(manifest)
    if incomplete:
        raise Fault("manifest", "; ".join(incomplete))
    cases = []
    passed = set()
    fault = None
    for case in CASES:
        remaining = deadline - 0
        if deadline <= 0 or deadline <= WORK_STOP_SECONDS:
            cases.append({"case": case["id"], "status": "unexecuted",
                          "detail": "inside the work-stop margin or past the budget"})
            continue
        if fault and mode == "counted":
            cases.append({"case": case["id"], "status": "skipped",
                          "kind": "latched",
                          "detail": "a counted run latched an earlier fault"})
            continue
        case_dir = make_directory(run_root, f"case-{case['id']}-")
        try:
            command = [stage["binary"], case["id"]]
            result = case_fn(command, case_environment(env, case_dir), 60)
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
                            f"(exit {result.returncode}); stdout tail: {result.stdout[-200:]}")
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
    clean = mode == "counted" and fault is None and len(passed) == len(CASES)
    return {
        "schema": SCHEMA,
        "stage": "inner",
        "mode": mode,
        "pre_stage_identity": pre_stage,
        "manifest": manifest,
        "run_root": run_root,
        "qualified_inner": bool(clean),
        "passed": sorted(passed),
        "cases": cases,
    }


def real_stage_fn(env, run_root):
    """The real in-guard stage: the managed Oxide build under the stage
    scrub, the artifact selected, its bytes digested."""
    environment = base.stage_environment(dict(env), os.path.join(run_root, "stage-tmp"))
    os.makedirs(environment["TMPDIR"], exist_ok=True)
    result = subprocess.run(STAGE_COMMAND, env=environment, capture_output=True,
                            text=True, timeout=240)
    if result.returncode != 0:
        raise Fault("stage",
                    f"the stage build exited {result.returncode}: {result.stderr[-400:]}")
    selected = None
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
            selected = document
    if selected is None:
        raise Fault("stage", "the stage build produced no journal_proof artifact")
    if selected.get("features") != [PROOF_FEATURE]:
        raise Fault("stage", "the staged artifact does not carry exactly the proof feature")
    binary = selected["executable"]
    digest = hashlib.sha256()
    with open(binary, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return {"binary": binary, "binary_sha256": digest.hexdigest()}


def real_identity_fn():
    """The real source binding, read from this checkout."""
    def identity():
        def git(arguments):
            result = subprocess.run(["git", *arguments], cwd=REPO_ROOT,
                                    capture_output=True, text=True, timeout=60)
            if result.returncode != 0:
                raise Fault("source",
                            f"git {' '.join(arguments)} failed: {result.stderr[-200:]}")
            return result.stdout.strip()

        def sha256_file(path):
            digest = hashlib.sha256()
            with open(path, "rb") as handle:
                for chunk in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(chunk)
            return digest.hexdigest()

        return source_identity(types.SimpleNamespace(
            git=git, sha256_file=sha256_file))
    return identity


def run_inner(arguments):
    """The guarded inner runner: invoked by the supervisor as the one guarded
    child, it inherits the supervisor's budget hook, stages inside the guard,
    mints the source-bound manifest here, runs the fixed cases, and prints
    one inner receipt. The outer composes the final receipt from it; the
    supervisor owns the group closure."""
    if not arguments.run_root:
        print(json.dumps({"schema": SCHEMA, "stage": "inner", "mode": arguments.inner,
                          "qualified_inner": False, "cases": [],
                          "detail": "the inner argv carries no owned run root"}),
              flush=True)
        return 1
    def real_digest_fn(path):
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        return digest.hexdigest()

    try:
        receipt = run_cases_inner(
            arguments.inner, arguments.run_root, dict(os.environ),
            inner_deadline(dict(os.environ), time.monotonic),
            lambda parent, prefix: tempfile.mkdtemp(prefix=prefix, dir=parent),
            real_stage_fn,
            lambda command, env, timeout: subprocess.run(
                command, env=env, capture_output=True, text=True, timeout=timeout),
            real_identity_fn(),
            real_digest_fn,
        )
    except Fault as error:
        receipt = {"schema": SCHEMA, "stage": "inner", "mode": arguments.inner,
                   "qualified_inner": False, "cases": [],
                   "fault": {"kind": error.kind, "detail": error.detail}}
    print(json.dumps(receipt), flush=True)
    if arguments.inner == "counted" and receipt["qualified_inner"]:
        return 0
    return 1


def run_native(mode, contract):
    """The actual native entry: verify the guard hooks, the pinned
    supervisor and the verified Stash route of the guard's own scratch, bind
    the run root there, and launch the one guarded child — the unchanged
    qualified supervisor around the inner runner, which stages inside the
    guard, mints the manifest, and runs the fixed cases. The outer composes
    and publishes the receipt from that guarded outcome; never qualified on
    any fault, an unverified boundary, or an inner receipt that is not
    exactly the fixed map."""
    receipt = {
        "schema": SCHEMA,
        "stage": "outer",
        "mode": mode,
        "qualified": False,
        "supervisor_sha256": base.SUPERVISOR_SHA256,
        "supervisor_verified": False,
        "route_uuid": ROUTE_UUID,
        "route_verified": False,
        "guard_environment": False,
        "verified_scratch_root": None,
        "guard_closure": "separate fact owned by the supervisor; this receipt "
                         "records the inner native outcome only",
        "manifest": None,
        "inner": None,
        "cases": [],
        "bounds": {"wall_seconds": LIMIT_SECONDS, "scratch_mib": SCRATCH_BUDGET_MIB,
                   "image": None, "work_stop_seconds": WORK_STOP_SECONDS,
                   "fit": "unmeasured"},
    }

    def finish():
        evidence = contract.env.get("OXIDE_PROOF_EVIDENCE")
        if not evidence:
            raise Fault("guard", "OXIDE_PROOF_EVIDENCE is absent; no receipt path")
        path = os.path.join(evidence, RECEIPT_NAME)
        try:
            contract.write_file(path, json.dumps(receipt, indent=2) + "\n")
        except OSError as error:
            raise Fault("report",
                        f"the receipt could not be published: {error}") from None
        return path

    # The guard contract: genuine Effigy/Nucleus admission supplies both hooks.
    if not contract.env.get("OXIDE_TASK_SCRATCH_ROOT") or not contract.env.get("OXIDE_PROOF_EVIDENCE"):
        raise Fault("guard", "the guard environment is incomplete; "
                             "OXIDE_TASK_SCRATCH_ROOT and OXIDE_PROOF_EVIDENCE are required")
    receipt["guard_environment"] = True

    try:
        found = contract.supervisor_digest()
    except OSError as error:
        raise Fault("supervisor", f"the pinned supervisor is unreadable: {error}") from None
    if found != base.SUPERVISOR_SHA256:
        raise Fault("supervisor",
                    f"the supervisor's bytes are {found}, not the pinned {base.SUPERVISOR_SHA256}")
    receipt["supervisor_verified"] = True

    # The guard's own scratch root is the verified Stash route: this is the
    # directory the supervisor adopts and the inner builds in.
    scratch_root = verified_scratch_root(contract)
    receipt["route_verified"] = True
    receipt["verified_scratch_root"] = scratch_root

    run_root = contract.make_directory(scratch_root, RUN_ROOT_PREFIX)

    inner_argv = inner_command(mode, run_root)
    guarded_command = supervisor_command(inner_argv, scratch_root)
    try:
        guarded = contract.run_child(guarded_command, dict(contract.env),
                                     LIMIT_SECONDS + 60)
    except subprocess.TimeoutExpired:
        raise Fault("reap",
                    "the guarded supervisor child did not finish inside its wall") from None

    # The guarded child's own inner outcome, parsed exactly once. The inner
    # staged inside the guard and minted the manifest; the outer binds it.
    inner = None
    for line in guarded.stdout.splitlines():
        try:
            document = json.loads(line)
        except ValueError:
            continue
        if isinstance(document, dict) and document.get("schema") == SCHEMA \
                and document.get("stage") == "inner":
            if inner is not None:
                raise Fault("child", "the guarded child answered more than once")
            inner = document
    if inner is None:
        raise Fault("child",
                    f"the guarded child printed no inner receipt (exit {guarded.returncode}); "
                    f"stdout tail: {guarded.stdout[-300:]}")
    receipt["inner"] = inner
    receipt["manifest"] = inner.get("manifest")
    receipt["cases"] = inner.get("cases", [])

    # Honesty. A faulted inner receipt is a legitimate recorded outcome: the
    # outer composes it as the failure it is, never qualified. An inner that
    # claims cases or qualification must name every fixed case exactly once
    # and carry the complete manifest it minted inside the guard.
    if inner.get("fault") and not inner.get("cases"):
        receipt["fault"] = inner["fault"]
    else:
        seen = [case.get("case") for case in receipt["cases"]]
        expected = [case["id"] for case in CASES]
        if sorted(seen) != sorted(expected):
            raise Fault("child",
                        f"the inner receipt's cases {seen} are not exactly the fixed map {expected}")
        if not receipt["manifest"]:
            raise Fault("child", "the inner receipt carries no manifest")
        incomplete = manifest_complete(receipt["manifest"])
        if incomplete:
            raise Fault("child", "; ".join(incomplete))

    receipt["qualified"] = bool(
        mode == "counted"
        and guarded.returncode == 0
        and inner.get("qualified_inner") is True
        and receipt["supervisor_verified"] and receipt["route_verified"]
    )
    path = finish()
    return receipt, path


# ---------- effect-free controls through the actual entry ----------

class FakeContract:
    """The external seams, scripted. Only executors, time, identity and
    storage are fakes; the composition they drive is the actual entry — the
    supervisor child is "executed" by really running the inner composition
    (in-guard stage, manifest mint, case loop) with the stage and driver
    children faked at the executor boundary."""

    def __init__(self):
        self.env = {"OXIDE_TASK_SCRATCH_ROOT": "/Volumes/Stash/scratch/barge/journal-guard",
                    "OXIDE_PROOF_EVIDENCE": "/guard/evidence",
                    "HOME": "/guard/home", "TMPDIR": "/guard/tmp"}
        self.written = {}
        self.directories = []
        self.sequence = []
        self.clock = None
        self.supervisor_digest_value = base.SUPERVISOR_SHA256
        self.route = {"present": True,
                      "path": "/Volumes/Stash/scratch/barge/journal-guard",
                      "realpath": "/Volumes/Stash/scratch/barge/journal-guard",
                      "volume_uuid": ROUTE_UUID,
                      "within_designated": True}
        self.inner_mode = None
        self.case_results = {}
        self.stage_result = self._stage_ok()
        self.source_digests = {}
        self.head = "0" * 40
        self.fail_report_write = False
        self.fail_stage = False
        self.fail_supervisor_child = False
        self.reap_fails = False
        self.last_guarded_command = None
        self.identity_calls = 0
        self.drift_after_identity = False
        self.staged_binary_digest_after_stage = None
        self.supervisor_child_exit = None

    def supervisor_digest(self):
        return self.supervisor_digest_value

    def route_facts(self, path):
        facts = dict(self.route)
        facts["path"] = path
        return facts

    def monotonic(self):
        if self.clock:
            return self.clock.pop(0) if len(self.clock) > 1 else self.clock[0]
        return 0.0

    @staticmethod
    def _stage_ok():
        line = ('{"reason":"compiler-artifact","target":{"kind":["example"],'
                '"crate_types":["bin"],"name":"' + DRIVER_NAME + '","edition":"2021"},'
                '"features":["' + PROOF_FEATURE + '"],"executable":"/stage/'
                + DRIVER_NAME + '"}')
        return types.SimpleNamespace(returncode=0, stdout=line + "\n", stderr="")

    def run_child(self, command, env, timeout):
        if len(command) > 2 and "bounded-test.py" in command[1]:
            # The guarded supervisor child. The seams faked here are the
            # kernel's time and the stage/driver children only: the inner
            # composition really runs — including the in-guard stage and the
            # manifest mint — from the argv the outer actually built.
            self.last_guarded_command = command
            self.sequence.append("guarded")
            inner_argv = command[command.index("--") + 1:]
            parsed = argument_parser().parse_args(inner_argv[2:])
            self.inner_mode = parsed.inner
            if self.fail_supervisor_child:
                return types.SimpleNamespace(returncode=1, stdout="", stderr="supervisor refused")
            if self.reap_fails:
                raise subprocess.TimeoutExpired(command, timeout)

            def stage_fn(stage_env, run_root):
                self.sequence.append("stage")
                if self.fail_stage:
                    raise Fault("stage", "the injected stage refusal")
                return {"binary": "/stage/" + DRIVER_NAME,
                        "binary_sha256": self.source_digests.get(
                            "/stage/" + DRIVER_NAME, "f" * 64)}

            def case_fn(case_command, case_env, timeout):
                case = case_command[-1]
                scripted = self.case_results.get(case)
                if scripted is not None:
                    return scripted
                return types.SimpleNamespace(
                    returncode=0,
                    stdout=json.dumps({"case": case, "status": "passed",
                                       "detail": "fake pass", "injections": []}) + "\n",
                    stderr="")

            try:
                receipt = run_cases_inner(
                    parsed.inner, parsed.run_root, env,
                    inner_deadline(env, lambda: 0.0),
                    lambda parent, prefix: f"{parent}/{prefix}{len(self.directories)}",
                    stage_fn, case_fn,
                    self.identity_fn, self.digest_fn,
                )
            except Fault as error:
                receipt = {"schema": SCHEMA, "stage": "inner", "mode": parsed.inner,
                           "qualified_inner": False, "cases": [],
                           "fault": {"kind": error.kind, "detail": error.detail}}
            clean = parsed.inner == "counted" and receipt["qualified_inner"]
            exit_code = self.supervisor_child_exit
            if exit_code is None:
                exit_code = 0 if clean else 1
            return types.SimpleNamespace(returncode=exit_code,
                                         stdout=json.dumps(receipt) + "\n", stderr="")
        return self._ok()

    def git(self, arguments):
        return self.head

    def sha256_file(self, path):
        if path == "/stage/" + DRIVER_NAME:
            # The staged binary's digest: the stage default unless the
            # replaced-binary control scripts a different re-hash answer.
            return self.staged_binary_digest_after_stage or "f" * 64
        return self.source_digests.get(path, "a" * 64)

    def identity_fn(self):
        """The faked source binding; scripted to drift after the first call
        so the drift comparison really executes against changed digests."""
        self.identity_calls += 1
        identity = source_identity(self)
        if self.drift_after_identity and self.identity_calls > 1:
            identity["store_source_sha256"] = "changed" * 8
        return identity

    def digest_fn(self, path):
        return self.sha256_file(path)

    def read_file(self, path):
        return self.source_digests.get(path, "")

    def make_directory(self, parent, prefix):
        path = f"{parent}/{prefix}{len(self.directories)}"
        self.directories.append(path)
        return path

    def write_file(self, path, text):
        if self.fail_report_write:
            raise OSError("the injected report failure")
        self.written[path] = text


def control_contract():
    """A contract whose source digests are internally consistent."""
    contract = FakeContract()
    contract.source_digests = {
        DRIVER_SOURCE: "d" * 64,
        STORE_SOURCE: "s" * 64,
        CARGO_LOCK: "l" * 64,
        TOOLCHAIN_FILE: "t" * 64,
    }
    return contract


def composition_controls():
    """Call the actual `run_native` entry: guard verification, the verified
    scratch route, the one guarded supervisor child (really running the
    inner composition — in-guard stage, manifest mint, case loop — with only
    the stage and driver children faked), verdict parsing, report
    publication and receipt composition."""
    problems = []

    def expect(condition, message):
        if not condition:
            problems.append(message)

    # Clean counted pass: the outer spawns the unchanged qualified supervisor
    # around the inner runner, the stage and manifest are minted inside the
    # guarded child (after the launch, never before it), and the receipt
    # qualifies.
    contract = control_contract()
    receipt, path = run_native("counted", contract)
    expect(receipt["qualified"] is True, "a clean counted pass did not qualify")
    expect(path.endswith(RECEIPT_NAME), "the receipt is not at the evidence path")
    expect(any(key.endswith(RECEIPT_NAME) for key in contract.written),
           "the receipt was not published")
    expect(contract.sequence[:2] == ["guarded", "stage"],
           "the stage did not run inside the guarded child, after the launch")
    expect(any(directory.startswith(
        "/Volumes/Stash/scratch/barge/journal-guard/journal-proof-")
        for directory in contract.directories),
        "the run root is not inside the verified guard scratch")
    expect(len([c for c in receipt["cases"] if c["status"] == "passed"]) == len(CASES),
           "the clean pass did not pass every case")
    guarded = contract.last_guarded_command
    expect(guarded is not None, "run_native never spawned the guarded supervisor child")
    expect(guarded and "bounded-test.py" in guarded[1],
           "the guarded child is not the pinned supervisor")
    inner_tail = guarded[guarded.index("--") + 1:]
    expect(inner_tail[:2] == ["python3", SCRIPT_PATH],
           "the guarded child does not wrap this script's inner runner")
    expect("--limit" in guarded and str(LIMIT_SECONDS) in guarded,
           "the guarded child does not carry the supervisor's limits")
    expect("/Volumes/Stash/scratch/barge/journal-guard" in guarded,
           "the guarded child does not adopt the verified scratch root as its parent")
    expect("--binary" not in inner_tail,
           "the inner argv carries a staged binary the outer cannot have minted")
    expect(contract.inner_mode == "counted", "the inner did not run the counted mode")
    manifest = receipt.get("manifest") or {}
    expect(manifest.get("features") == [PROOF_FEATURE]
           and len(manifest.get("binary_sha256", "")) >= 32
           and len(manifest.get("head", "")) >= 32,
           "the outer receipt does not bind the inner's in-guard manifest")
    expect(receipt["inner"] is not None and receipt["inner"]["qualified_inner"] is True,
           "the outer receipt does not embed the inner's own outcome")

    # Supervisor unverified: the pin disagrees, and the entry refuses before
    # anything runs.
    contract = control_contract()
    contract.supervisor_digest_value = "b" * 64
    try:
        run_native("counted", contract)
        problems.append("an unverified supervisor did not fault")
    except Fault as error:
        expect(error.kind == "supervisor", "an unverified supervisor faulted with the wrong kind")
    expect(not contract.last_guarded_command,
           "an unverified supervisor still launched a guarded child")

    # Guard environment absent: refused, never qualified.
    contract = control_contract()
    contract.env = {}
    try:
        run_native("counted", contract)
        problems.append("a missing guard did not fault")
    except Fault as error:
        expect(error.kind == "guard", "a missing guard faulted with the wrong kind")

    # Route mismatch: the guard's scratch is not the verified Stash volume.
    contract = control_contract()
    contract.route["volume_uuid"] = "00000000-0000-0000-0000-000000000000"
    try:
        run_native("counted", contract)
        problems.append("a route mismatch did not fault")
    except Fault as error:
        expect(error.kind == "route", "a route mismatch faulted with the wrong kind")
    expect(not contract.last_guarded_command,
           "a route mismatch still launched a guarded child")

    # Route escape: the guard's scratch resolves outside the designated barge
    # scratch. Refused before the launch.
    contract = control_contract()
    contract.route["within_designated"] = False
    try:
        run_native("counted", contract)
        problems.append("a route escape did not fault")
    except Fault as error:
        expect(error.kind == "route", "a route escape faulted with the wrong kind")
    expect(not contract.last_guarded_command,
           "a route escape still launched a guarded child")

    # Case fault: a failing case fails the inner, latches the counted run,
    # and the outer composes an unqualified receipt embedding the outcome.
    contract = control_contract()
    contract.case_results = {"J1": types.SimpleNamespace(
        returncode=1,
        stdout=json.dumps({"case": "J1", "status": "failed",
                           "detail": "injected: refused"}) + "\n",
        stderr="")}
    receipt, _ = run_native("counted", contract)
    statuses = {c["case"]: c["status"] for c in receipt["cases"]}
    expect(statuses["J1"] == "failed", "the failing case was not recorded")
    expect(statuses["J12"] == "skipped", "a latched counted run did not skip the rest")
    expect(receipt["qualified"] is False, "a latched run produced a qualified receipt")
    expect(contract.sequence[:2] == ["guarded", "stage"],
           "the faulting run did not stage inside the guarded child")

    # Bad child reply: no verdict for the case fails the inner honestly.
    contract = control_contract()
    contract.case_results = {"J1": types.SimpleNamespace(returncode=0, stdout="{}\n", stderr="")}
    receipt, _ = run_native("counted", contract)
    failed = [c for c in receipt["cases"] if c["case"] == "J1"]
    expect(failed and failed[0]["status"] == "failed",
           "a bad child reply was not recorded")
    expect(receipt["qualified"] is False, "a bad child reply produced a qualified receipt")

    # Duplicate and unknown verdicts refuse inside the inner composition.
    contract = control_contract()
    duplicate = json.dumps({"case": "J1", "status": "passed", "detail": "x"})
    contract.case_results = {"J1": types.SimpleNamespace(
        returncode=0, stdout=duplicate + "\n" + duplicate, stderr="")}
    receipt, _ = run_native("counted", contract)
    expect(receipt["qualified"] is False, "a duplicate verdict produced a qualified receipt")
    contract = control_contract()
    contract.case_results = {"J1": types.SimpleNamespace(
        returncode=0,
        stdout=json.dumps({"case": "J99", "status": "passed", "detail": "x"}) + "\n",
        stderr="")}
    receipt, _ = run_native("counted", contract)
    expect(receipt["qualified"] is False, "an unknown-case verdict produced a qualified receipt")

    # Stage refusal inside the guard: the inner faults, the outer composes an
    # unqualified receipt from the guarded outcome.
    contract = control_contract()
    contract.fail_stage = True
    receipt, _ = run_native("counted", contract)
    expect(receipt["qualified"] is False, "a stage refusal produced a qualified receipt")
    expect(receipt.get("fault", {}).get("kind") == "stage",
           "the stage refusal was not recorded as the guarded fault")
    expect(contract.sequence[:2] == ["guarded", "stage"],
           "the stage refusal did not happen inside the guarded child")

    # Reap: a guarded child that never finishes faults as a reap failure, and
    # nothing qualifies.
    contract = control_contract()
    contract.reap_fails = True
    try:
        receipt, _ = run_native("counted", contract)
        expect(receipt["qualified"] is False, "an unreaped child produced a qualified receipt")
    except Fault as error:
        expect(error.kind == "reap", "an unreaped child faulted with the wrong kind")

    # Supervisor child refusal: nonzero with no inner receipt is a child fault.
    contract = control_contract()
    contract.fail_supervisor_child = True
    try:
        receipt, _ = run_native("counted", contract)
        expect(receipt["qualified"] is False, "a refused supervisor produced a qualified receipt")
    except Fault as error:
        expect(error.kind == "child", "a refused supervisor faulted with the wrong kind")

    # Deadline inside the inner: the guarded budget expired, every case is
    # recorded unexecuted, and the receipt never qualifies.
    contract = control_contract()
    contract.env["OXIDE_PROOF_DEADLINE_NS"] = "0"
    receipt, _ = run_native("counted", contract)
    expect(len(receipt["cases"]) == len(CASES),
           "the expired budget did not account for every case")
    expect(all(c["status"] == "unexecuted" for c in receipt["cases"]),
           "an expired budget did not record the unexecuted cases")
    expect(receipt["qualified"] is False, "an expired budget produced a qualified receipt")
    expect(contract.sequence[:2] == ["guarded", "stage"],
           "the expired-budget run did not stage inside the guarded child")

    # Diagnostic: keep-going records every case and always qualifies false.
    contract = control_contract()
    contract.case_results = {"J3": types.SimpleNamespace(
        returncode=1,
        stdout=json.dumps({"case": "J3", "status": "failed",
                           "detail": "injected: refused"}) + "\n",
        stderr="")}
    receipt, _ = run_native("diagnostic", contract)
    expect(len([c for c in receipt["cases"] if c["status"] in ("passed", "failed")]) == len(CASES),
           "the diagnostic did not record every case")
    expect(receipt["qualified"] is False, "a diagnostic receipt claimed qualified")

    # Manifest/source drift: the store digest changes between the pre-stage
    # capture and the manifest mint, and the entry faults as drift before any
    # case runs.
    contract = control_contract()
    contract.drift_after_identity = True
    try:
        receipt, _ = run_native("counted", contract)
        expect(receipt["qualified"] is False, "drift produced a qualified receipt")
        expect(receipt.get("fault", {}).get("kind") == "drift",
               "the identity drift was not recorded as the guarded fault")
        expect(not any(c["status"] == "passed" for c in receipt["cases"]),
               "a drifted run still passed cases")
    except Fault as error:
        expect(error.kind == "drift", "an identity drift faulted with the wrong kind")

    # Replaced staged binary: the re-hash before the first case disagrees
    # with the manifest's digest, and the entry faults as drift.
    contract = control_contract()
    contract.staged_binary_digest_after_stage = "c" * 64
    try:
        receipt, _ = run_native("counted", contract)
        expect(receipt["qualified"] is False, "a replaced binary produced a qualified receipt")
        expect(receipt.get("fault", {}).get("kind") == "drift",
               "the replaced binary was not recorded as drift")
        expect(not receipt["cases"] or
               all(c["status"] != "passed" for c in receipt["cases"]),
               "a replaced binary still let cases pass")
    except Fault as error:
        expect(error.kind == "drift", "a replaced binary faulted with the wrong kind")

    # Guard not closed: the inner receipt is clean, but the guarded child
    # exits nonzero - the supervisor latched a custody fault - and the outer
    # composes the honest unqualified receipt. Qualification never rests on
    # the inner outcome alone.
    contract = control_contract()
    contract.supervisor_child_exit = 1
    receipt, _ = run_native("counted", contract)
    expect(receipt["qualified"] is False,
           "a nonzero guarded exit with a clean inner receipt produced a qualified receipt")
    expect(receipt["inner"] is not None and receipt["inner"]["qualified_inner"] is True,
           "the clean inner outcome was not preserved in the receipt")

    # Report failure: an unwritable receipt faults and never qualifies.
    contract = control_contract()
    contract.fail_report_write = True
    try:
        run_native("counted", contract)
        problems.append("a failed report did not fault")
    except Fault as error:
        expect(error.kind == "report", "a report failure faulted with the wrong kind")

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
    designated = os.path.realpath(DESIGNATED_SCRATCH)
    real = facts.get("realpath", "")
    facts["within_designated"] = (
        real == designated or real.startswith(designated + os.sep))
    return route_problems(facts)


def argv_shape_problems():
    """The outer guarded command must end in an inner argv this script's own
    parser accepts, binding the mode and the verified owned run root, and
    must let the supervisor adopt that same verified root."""
    problems = []
    command = supervisor_command(
        inner_command("counted", "/Volumes/Stash/scratch/barge/journal-guard/run"),
        "/Volumes/Stash/scratch/barge/journal-guard")
    tail = command[command.index("--") + 1:]
    if tail[0] != "python3" or tail[1] != SCRIPT_PATH:
        problems.append("the guarded command does not run this script's inner runner")
    try:
        parsed = argument_parser().parse_args(tail[2:])
    except SystemExit:
        problems.append("the guarded inner argv does not parse")
        return problems
    if parsed.inner != "counted" or not parsed.run_root:
        problems.append("the guarded inner argv does not bind the mode and run root")
    if "--limit" not in command or str(LIMIT_SECONDS) not in command:
        problems.append("the guarded command does not carry the supervisor's limits")
    parent_index = command.index("--root-parent")
    if command[parent_index + 1] != "/Volumes/Stash/scratch/barge/journal-guard":
        problems.append("the supervisor does not adopt the verified scratch root")
    return problems


def execution_readiness():
    """Whether every executable prerequisite is present on this host. The
    plan reports `execution_ready` honestly: false with reasons while any
    prerequisite is absent, true only at complete source readiness — the
    entry composition controls are part of readiness, so a counted or
    diagnostic pass that cannot compose is never reported ready."""
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
    composition controls through the actual entry with faked seams."""
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
    parser.add_argument("--run-root")
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
              "entry composition controls passed, no image, no scratch, no child process)")
        return 0
    if arguments.plan:
        print(json.dumps(plan_document(), indent=2))
        return 0
    if arguments.inner is not None:
        # The guarded inner runner: all twelve fixed cases, executed directly
        # under the supervisor's budget, one inner receipt on stdout.
        return run_inner(arguments)
    if arguments.mode in ("counted", "diagnostic"):
        # The owner-activated entry. This is real: it verifies the supervisor
        # and route, stages inside the guard, runs every case and publishes
        # the receipt. It is never invoked by an ordinary selector.
        contract = Contract.from_environment()
        try:
            receipt, path = run_native(arguments.mode, contract)
        except Fault as error:
            print(f"fault ({error.kind}): {error.detail}", file=sys.stderr)
            return 1
        print(json.dumps({"receipt": path, "qualified": receipt["qualified"],
                          "mode": receipt["mode"]}))
        # The diagnostic proves nothing on its own: always nonzero.
        if arguments.mode == "diagnostic":
            return 1
        return 0 if receipt["qualified"] else 1
    parser.error("name a mode: counted or diagnostic (or --inner, --self-check, --plan)")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
