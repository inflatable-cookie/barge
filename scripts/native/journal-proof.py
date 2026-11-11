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
     "the inner runner exports a private home, cargo home and state directory "
     "under the guard's owned root and passes them to every case child"),
    ("private account resolution; complete before and after real-state digest",
     "n/a for the store surface: the journal never reads account facts; the "
     "driver resolves its state directory from BARGE_STATE_DIR alone"),
    ("TMPDIR, TMP and TEMP inside the owned root; inherited injection removed",
     "the inner runner overrides all three to directories under the owned root "
     "and clears inherited interposers"),
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

        return Contract(environment, run_child,
                        lambda: base.sha256_file(base.supervisor_path()),
                        lambda: base.scratch_route_facts(),
                        git, sha256_file, read_file,
                        time.monotonic, make_directory, write_file)


def supervisor_path():
    return base.supervisor_path()


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


def stage_driver(contract, deadline):
    """Stage the proof build through the managed Oxide build and select the
    driver's own artifact. The stage command's artifact lines name the
    example target, its feature and the executable; anything else is a
    refusal."""
    environment = base.stage_environment(dict(contract.env), None)
    environment["TMPDIR"] = environment.get("TMPDIR") or contract.env.get("TMPDIR", "/tmp")
    if contract.monotonic() >= deadline:
        raise Fault("deadline", "the stage did not start inside the budget")
    try:
        result = contract.run_child(STAGE_COMMAND, environment, 240)
    except subprocess.TimeoutExpired as error:
        raise Fault("stage", f"the stage build did not finish: {error}") from None
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
    binary_sha256 = contract.sha256_file(binary)
    return {"binary": binary, "binary_sha256": binary_sha256}


def source_drift_problems(contract, identity):
    """The staged source must be the manifest's source: a changed driver or
    store between staging and the launch is drift, and drift never qualifies."""
    problems = []
    checks = (
        ("driver_source_sha256", DRIVER_SOURCE),
        ("store_source_sha256", STORE_SOURCE),
        ("cargo_lock_sha256", CARGO_LOCK),
        ("toolchain_sha256", TOOLCHAIN_FILE),
    )
    for field, path in checks:
        if contract.sha256_file(path) != identity[field]:
            problems.append(f"{os.path.basename(path)} changed after the stage")
    if contract.git(["rev-parse", "HEAD"]) != identity["head"]:
        problems.append("HEAD changed after the stage")
    return problems


def case_environment(inner_env, case_dir):
    """The case child's environment: the stage scrub, then the fixture
    bindings applied after it, so the scrub cannot drop a binding."""
    scrubbed = base.stage_environment(dict(inner_env), case_dir)
    scrubbed.update({
        "BARGE_PROOF_CASE_DIR": case_dir,
        "TMPDIR": case_dir,
        "TMP": case_dir,
        "TEMP": case_dir,
    })
    return scrubbed


def inner_command(mode, binary, binary_sha256, run_root):
    """The guarded inner argv: this script's inner runner, bound to the
    staged binary, the manifest's digest of it, and this run's owned root."""
    return [
        "python3", SCRIPT_PATH, "--inner", mode,
        "--binary", binary,
        "--binary-sha256", binary_sha256,
        "--run-root", run_root,
    ]


def supervisor_command(inner_argv):
    """The unchanged qualified supervisor wrapping the inner argv: the one
    guarded child a native run spawns."""
    return [
        sys.executable, base.supervisor_path(),
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", DESIGNATED_SCRATCH,
        "--", *inner_argv,
    ]


def route_problems(facts):
    """Why the designated scratch route is not the verified private Stash."""
    problems = []
    if not facts.get("present"):
        problems.append(f"the designated scratch {facts.get('path')} is absent")
    if facts.get("volume_uuid") != ROUTE_UUID:
        problems.append(
            f"the designated scratch's volume is {facts.get('volume_uuid')}, "
            f"not the verified private Stash {ROUTE_UUID}"
        )
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


def run_cases_inner(mode, binary, binary_sha256, run_root, exec_case, monotonic,
                    make_directory, env):
    """The inner composition: the twelve fixed case groups, each one direct
    driver child in its own fresh fixture directory under the owned run root,
    counted fail-fast or diagnostic keep-going, and one inner receipt.

    This is the function the `--inner` runner calls with real child
    execution, and the effect-free controls call with only the case executor
    faked — the composition itself is never a fake.
    """
    deadline = inner_deadline(env, monotonic)
    cases = []
    passed = set()
    fault = None
    for case in CASES:
        if deadline - monotonic() <= 0:
            cases.append({"case": case["id"], "status": "unexecuted",
                          "detail": "the guarded budget expired before this case"})
            continue
        if fault and mode == "counted":
            cases.append({"case": case["id"], "status": "skipped",
                          "kind": "latched",
                          "detail": "a counted run latched an earlier fault"})
            continue
        if fault and deadline - monotonic() <= WORK_STOP_SECONDS:
            cases.append({"case": case["id"], "status": "unexecuted",
                          "detail": "inside the work-stop margin"})
            continue
        case_dir = make_directory(run_root, f"case-{case['id']}-")
        try:
            command = [binary, case["id"]]
            result = exec_case(command, case_environment(env, case_dir), 60)
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
        "binary_sha256": binary_sha256,
        "run_root": run_root,
        "qualified_inner": bool(clean),
        "passed": sorted(passed),
        "cases": cases,
    }


def run_inner(arguments):
    """The guarded inner runner: invoked by the supervisor as the one guarded
    child, it stages nothing and verifies nothing about the host — it runs
    the fixed cases through the staged binary and prints one inner receipt.
    The outer composes the final receipt from it; the supervisor owns the
    group closure."""
    if not arguments.binary or not arguments.run_root:
        print(json.dumps({"schema": SCHEMA, "stage": "inner", "mode": arguments.inner,
                          "qualified_inner": False, "cases": [],
                          "detail": "the inner argv carries no staged binary or run root"}),
              flush=True)
        return 1
    receipt = run_cases_inner(
        arguments.inner, arguments.binary, arguments.binary_sha256 or "",
        arguments.run_root,
        lambda command, env, timeout: subprocess.run(
            command, env=env, capture_output=True, text=True, timeout=timeout),
        time.monotonic,
        lambda parent, prefix: tempfile.mkdtemp(prefix=prefix, dir=parent),
        dict(os.environ),
    )
    print(json.dumps(receipt), flush=True)
    if arguments.inner == "counted" and receipt["qualified_inner"]:
        return 0
    return 1


def run_native(mode, contract):
    """The actual native entry: verify, stage, bind, launch the unchanged
    qualified supervisor around the inner runner, and compose the receipt
    from the guarded child's own inner outcome. Never qualified on any
    fault, drift, unverified supervisor or route, a bad or missing child
    reply, duplicate or unknown cases, or an expired budget."""
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
        "guard_closure": "separate fact owned by the supervisor; this receipt "
                         "records the inner native outcome only",
        "head": None,
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

    route_faults = route_problems(contract.route_facts())
    if route_faults:
        raise Fault("route", "; ".join(route_faults))
    receipt["route_verified"] = True

    run_root = contract.make_directory(contract.env["OXIDE_TASK_SCRATCH_ROOT"],
                                       RUN_ROOT_PREFIX)

    stage = stage_driver(contract, contract.monotonic() + LIMIT_SECONDS)
    identity = source_identity(contract)
    identity["binary_sha256"] = stage["binary_sha256"]
    receipt["manifest"] = identity
    receipt["head"] = identity["head"]

    drift = source_drift_problems(contract, identity)
    if drift:
        raise Fault("drift", "; ".join(drift))

    inner_argv = inner_command(mode, stage["binary"], stage["binary_sha256"], run_root)
    guarded_command = supervisor_command(inner_argv)
    try:
        guarded = contract.run_child(guarded_command, dict(contract.env),
                                     LIMIT_SECONDS + 60)
    except subprocess.TimeoutExpired:
        raise Fault("reap",
                    "the guarded supervisor child did not finish inside its wall") from None

    # The guarded child's own inner outcome, parsed exactly once.
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
    receipt["cases"] = inner.get("cases", [])

    # Honesty: the inner receipt must name every fixed case exactly once, no
    # unknown case, and carry this run's own staged binary digest.
    seen = [case.get("case") for case in receipt["cases"]]
    expected = [case["id"] for case in CASES]
    if sorted(seen) != sorted(expected):
        raise Fault("child",
                    f"the inner receipt's cases {seen} are not exactly the fixed map {expected}")
    if inner.get("binary_sha256") != stage["binary_sha256"]:
        raise Fault("child",
                    "the inner receipt does not carry this run's staged binary digest")

    receipt["qualified"] = bool(
        mode == "counted"
        and guarded.returncode == 0
        and inner.get("qualified_inner") is True
        and receipt["supervisor_verified"] and receipt["route_verified"]
    )
    path = finish()
    return receipt, path


def run_inner_main(arguments):
    """Dispatch the guarded inner runner (kept beside run_native for the argv
    guard to execute)."""
    return run_inner(arguments)


# ---------- effect-free controls through the actual entry ----------

class FakeContract:
    """The external seams, scripted. Only executors, time, identity and
    storage are fakes; the composition they drive is the actual entry — the
    supervisor child is "executed" by really running the inner composition
    with the case executor faked at the driver-child boundary."""

    def __init__(self):
        self.env = {"OXIDE_TASK_SCRATCH_ROOT": "/guard/scratch",
                    "OXIDE_PROOF_EVIDENCE": "/guard/evidence",
                    "HOME": "/guard/home", "TMPDIR": "/guard/tmp"}
        self.written = {}
        self.directories = []
        self.clock = None
        self.supervisor_digest_value = base.SUPERVISOR_SHA256
        self.route = {"present": True, "path": DESIGNATED_SCRATCH,
                      "volume_uuid": ROUTE_UUID}
        self.stage_result = self._stage_ok()
        self.supervisor_child = None
        self.case_results = {}
        self.inner_mode = None
        self.source_digests = {}
        self.head = "0" * 40
        self.fail_report_write = False
        self.fail_stage = False
        self.fail_supervisor_child = False
        self.reap_fails = False
        self.last_guarded_command = None

    def supervisor_digest(self):
        return self.supervisor_digest_value

    def route_facts(self):
        return dict(self.route)

    def monotonic(self):
        if self.clock:
            return self.clock.pop(0) if len(self.clock) > 1 else self.clock[0]
        return 0.0

    @staticmethod
    def _ok():
        return types.SimpleNamespace(returncode=0, stdout="", stderr="")

    def _stage_ok(self):
        line = ('{"reason":"compiler-artifact","target":{"kind":["example"],'
                '"crate_types":["bin"],"name":"' + DRIVER_NAME + '","edition":"2021"},'
                '"features":["' + PROOF_FEATURE + '"],"executable":"/stage/'
                + DRIVER_NAME + '"}')
        return types.SimpleNamespace(returncode=0, stdout=line + "\n", stderr="")

    def run_child(self, command, env, timeout):
        if command[:2] == ["oxide", "build"]:
            if self.fail_stage:
                return types.SimpleNamespace(returncode=101, stdout="", stderr="staging refused")
            return self.stage_result
        if len(command) > 2 and "bounded-test.py" in command[1]:
            # The guarded supervisor child. The seams faked here are the
            # kernel's time and the driver child only: the inner composition
            # really runs, from the argv the outer actually built.
            self.last_guarded_command = command
            inner_argv = command[command.index("--") + 1:]
            parsed = argument_parser().parse_args(inner_argv[2:])
            self.inner_mode = parsed.inner
            if self.fail_supervisor_child:
                return types.SimpleNamespace(returncode=1, stdout="", stderr="supervisor refused")
            if self.reap_fails:
                raise subprocess.TimeoutExpired(command, timeout)
            receipt = run_cases_inner(
                parsed.inner, parsed.binary, parsed.binary_sha256, parsed.run_root,
                self.execute_case, lambda: 0.0,
                lambda parent, prefix: f"{parent}/{prefix}{len(self.directories)}",
                self.env,
            )
            clean = parsed.inner == "counted" and receipt["qualified_inner"]
            return types.SimpleNamespace(returncode=0 if clean else 1,
                                         stdout=json.dumps(receipt) + "\n", stderr="")
        return self._ok()

    def execute_case(self, command, env, timeout):
        """The faked driver child: the only external seam the controls
        substitute, at the same signature the real inner uses."""
        case = command[-1]
        scripted = self.case_results.get(case)
        if scripted is not None:
            return scripted
        return types.SimpleNamespace(
            returncode=0,
            stdout=json.dumps({"case": case, "status": "passed",
                               "detail": "fake pass", "injections": []}) + "\n",
            stderr="")

    def git(self, arguments):
        return self.head

    def sha256_file(self, path):
        return self.source_digests.get(path, "a" * 64)

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
    """Call the actual `run_native` entry: launch, stage, the guarded
    supervisor child (really running the inner composition with the driver
    children faked), verdict parsing, report publication and receipt
    composition — with only the external executors, clock, identity and
    storage faked."""
    problems = []

    def expect(condition, message):
        if not condition:
            problems.append(message)

    # Clean counted pass: the outer spawns the unchanged qualified supervisor
    # around the inner runner, every case passes, the receipt qualifies.
    contract = control_contract()
    receipt, path = run_native("counted", contract)
    expect(receipt["qualified"] is True, "a clean counted pass did not qualify")
    expect(path.endswith(RECEIPT_NAME), "the receipt is not at the evidence path")
    expect(any(key.endswith(RECEIPT_NAME) for key in contract.written),
           "the receipt was not published")
    expect(any(directory.startswith("/guard/scratch/journal-proof-")
               for directory in contract.directories),
           "the run root is not inside the guard's owned scratch")
    expect(len([c for c in receipt["cases"] if c["status"] == "passed"]) == len(CASES),
           "the clean pass did not pass every case")
    # The launch shape: the guarded command is the unchanged qualified
    # supervisor wrapping this script's inner argv, and the inner really ran.
    guarded = contract.last_guarded_command
    expect(guarded is not None, "run_native never spawned the guarded supervisor child")
    expect(guarded and "bounded-test.py" in guarded[1],
           "the guarded child is not the pinned supervisor")
    expect(guarded and guarded[guarded.index("--") + 1:][:2] == ["python3", SCRIPT_PATH],
           "the guarded child does not wrap this script's inner runner")
    expect(guarded and "--limit" in guarded and str(LIMIT_SECONDS) in guarded,
           "the guarded child does not carry the supervisor's limits")
    expect(guarded and DESIGNATED_SCRATCH in guarded,
           "the guarded child does not name the verified scratch parent")
    expect(contract.inner_mode == "counted", "the inner did not run the counted mode")
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

    # Route mismatch: the scratch's volume is not the verified Stash.
    contract = control_contract()
    contract.route = {"present": True, "path": DESIGNATED_SCRATCH,
                      "volume_uuid": "00000000-0000-0000-0000-000000000000"}
    try:
        run_native("counted", contract)
        problems.append("a route mismatch did not fault")
    except Fault as error:
        expect(error.kind == "route", "a route mismatch faulted with the wrong kind")
    expect(not contract.last_guarded_command,
           "a route mismatch still launched a guarded child")

    # Stage refusal: the build fails, and no guarded child runs.
    contract = control_contract()
    contract.fail_stage = True
    try:
        run_native("counted", contract)
        problems.append("a refused stage did not fault")
    except Fault as error:
        expect(error.kind == "stage", "a stage refusal faulted with the wrong kind")
    expect(not contract.last_guarded_command, "a refused stage still launched the guard")

    # Wrong artifact: the stage output names another target, and the selector
    # refuses it.
    contract = control_contract()
    wrong = ('{"reason":"compiler-artifact","target":{"kind":["bin"],'
             '"crate_types":["bin"],"name":"barge","edition":"2021"},'
             '"features":["' + PROOF_FEATURE + '"],"executable":"/stage/barge"}')
    contract.stage_result = types.SimpleNamespace(returncode=0, stdout=wrong + "\n", stderr="")
    try:
        run_native("counted", contract)
        problems.append("a wrong staged artifact did not fault")
    except Fault as error:
        expect(error.kind == "stage", "a wrong artifact faulted with the wrong kind")

    # Source drift: the store digest changes after the manifest mints it, and
    # the entry refuses before launching the guarded child.
    contract = control_contract()
    store_calls = {"count": 0}
    original_sha = contract.sha256_file

    def drifting_sha(path):
        if path == STORE_SOURCE:
            store_calls["count"] += 1
            if store_calls["count"] > 1:
                return "changed" * 8
        return original_sha(path)
    contract.sha256_file = drifting_sha
    try:
        receipt, _ = run_native("counted", contract)
        expect(receipt["qualified"] is False, "drift produced a qualified receipt")
    except Fault as error:
        expect(error.kind == "drift", "a drift faulted with the wrong kind")
    expect(not contract.last_guarded_command, "drift still launched the guarded child")

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
    contract.clock = [0.0] * 6
    contract.env["OXIDE_PROOF_DEADLINE_NS"] = "0"
    receipt, _ = run_native("counted", contract)
    expect(len(receipt["cases"]) == len(CASES),
           "the expired budget did not account for every case")
    expect(all(c["status"] == "unexecuted" for c in receipt["cases"]),
           "an expired budget did not record the unexecuted cases")
    expect(receipt["qualified"] is False, "an expired budget produced a qualified receipt")

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
    """The regression names this proof carries, read from the real source."""
    names = set()
    with open(STORE_SOURCE, "r", encoding="utf-8") as handle:
        for line in handle:
            stripped = line.strip()
            if stripped.startswith("fn ") and "(" in stripped:
                names.add(stripped[3:].split("(")[0].strip())
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
    return route_problems(facts)


def argv_shape_problems():
    """The outer guarded command must end in an inner argv this script's own
    parser accepts, binding mode, staged binary, digest and run root."""
    problems = []
    command = supervisor_command(inner_command("counted", "/stage/" + DRIVER_NAME,
                                               "a" * 64, "/guard/scratch/run"))
    tail = command[command.index("--") + 1:]
    if tail[0] != "python3" or tail[1] != SCRIPT_PATH:
        problems.append("the guarded command does not run this script's inner runner")
    try:
        parsed = argument_parser().parse_args(tail[2:])
    except SystemExit:
        problems.append("the guarded inner argv does not parse")
        return problems
    if parsed.inner != "counted" or not parsed.binary or not parsed.run_root:
        problems.append("the guarded inner argv does not bind mode, binary and run root")
    if "--limit" not in command or str(LIMIT_SECONDS) not in command:
        problems.append("the guarded command does not carry the supervisor's limits")
    if DESIGNATED_SCRATCH not in command:
        problems.append("the guarded command does not name the verified scratch parent")
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
    parser.add_argument("--binary")
    parser.add_argument("--binary-sha256")
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
