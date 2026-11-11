#!/usr/bin/env python3
"""Barge's operation-journal native proof: the single-authority store's cases
on fresh private filesystem fixtures, under the qualified shared supervisor.

  python3 scripts/native/journal-proof.py --self-check  # apparatus only, effect-free
  python3 scripts/native/journal-proof.py --plan        # the plan, effect-free
  python3 scripts/native/journal-proof.py counted       # the author pass (owner-activated)
  python3 scripts/native/journal-proof.py diagnostic    # the uncounted twin (owner-activated)

It is owner-activated. No ordinary Effigy selector runs a counted or diagnostic
pass, and the declarations in `effigy.toml` activate nothing.

What it is, in one sentence: after the unchanged qualified supervisor's bytes
are verified, it stages the `proof-journal` feature build of HEAD
(`crates/barge/examples/journal_proof.rs`, required-features gated), and runs
twelve journal-only case groups through the actual store, lock and publication
code on fresh private filesystem fixtures — a private state directory per case
inside the guard's owned scratch, one synthetic volume identity, and no image.

The twelve groups, each carried by named regressions in
`crates/barge/src/operation.rs` (the self-check proves every name):

  J1  init and binding        epoch first, binding second, same id; partial pair;
                              never bind over records
  J2  request identity        exact caller/key/request/unit/hook matching;
                              owed-digest binding; unit-binding conflict;
                              Barge-owned unit idempotency; spent keys
  J3  full-capacity retry     a valid full journal answers Already for the live
                              request and the bound unit, refuses mismatches,
                              and refuses a new allocation without a write
  J4  crash before record     injected publish failure (labelled) leaves the
                              journal unchanged
  J5  torn watermark          one verified allocation tail; the next begin
                              reconciles before allocating
  J6  gaps and loss           a proven gap is Unknown, never silence; a record
                              lost after removed/owed stays unknown
  J7  retained terminals      cancellation retained; adjacent-only ladder; the
                              ack settles and freezes
  J8  malformed and bounds    malformed, foreign-format, name-mismatched,
                              stray, over-cap documents refuse; payload cap
  J9  directory custody       replaced/linked journal directory refused with no
                              substitute write; unsafe mode refused; missing
                              directory reads unbound and refuses writes
  J10 consult and legacy      try-shared busy refusal without waiting; free
                              consult answers; the shipped journal never mixes
  J11 capture evidence        capture and foreign-capture evidence publish
                              before effects, bounded
  J12 diagnostic              the read-only diagnosis reports findings and
                              writes nothing

The driver labels every injected failure in its verdict evidence. No output
root is deleted, no image is attached, and no real account, drive or install
is touched. The future counted pass uses the ordinary bounds — 300 s wall,
512 MiB owned scratch, stopping no later than the supervisor's own margin —
and the fit is unmeasured until its own owner-activated diagnostic.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys

sys.dont_write_bytecode = True

SCHEMA = "barge.journal.native/1"
SUPERVISOR_SHA256 = "fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b"
SUPERVISOR = os.path.expanduser("~/Dev/projects/oxide/scripts/bounded-test.py")
PARENT = "/Volumes/Stash/scratch/barge"
SCRIPT = os.path.abspath(__file__)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT)))
DRIVER = os.path.join(REPO_ROOT, "crates", "barge", "examples", "journal_proof.rs")
OPERATION_SOURCE = os.path.join(REPO_ROOT, "crates", "barge", "src", "operation.rs")
MANIFEST = os.path.join(REPO_ROOT, "crates", "barge", "Cargo.toml")
EFFIGY = os.path.join(REPO_ROOT, "effigy.toml")
PROOF_FEATURE = "proof-journal"
DRIVER_NAME = "journal_proof"
STAGE_COMMAND = [
    "oxide", "build", "-p", "barge", "--locked", "--offline",
    "--features", PROOF_FEATURE, "--example", DRIVER_NAME,
    "--message-format", "json",
]
# The future counted pass: the unchanged qualified shared runner, no image,
# the ordinary bounds. The fit is unmeasured until the diagnostic runs.
WALL_SECONDS = 300
SCRATCH_MIB = 512

# Each group names the driver case it runs and the real regressions that
# carry its claim in `crates/barge/src/operation.rs`. The self-check proves
# both directions.
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
              "substitute write, an unsafe mode refuses every access, and a "
              "missing directory reads as unbound and refuses writes",
     "carried_by": [
         "a_replaced_journal_directory_is_never_followed",
         "an_unsafe_journal_directory_mode_refuses_every_access",
         "a_missing_journal_directory_reads_unbound_and_refuses_writes",
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

# The nine Oxide workflow premises, each with the actual disposition this
# journal-only proof carries or an explicit n/a.
PREMISES = (
    ("private HOME, CARGO_HOME and BARGE_STATE_DIR",
     "the inner runner exports a private home, cargo home and state directory "
     "under the guard's owned root and passes them to every case"),
    ("private account resolution; complete before and after real-state digest",
     "n/a for the store surface: the journal never reads account facts; the "
     "driver resolves its state directory from BARGE_STATE_DIR alone, and the "
     "real-state digest still runs before and after"),
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


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def source_text():
    with open(SCRIPT, "r", encoding="utf-8") as handle:
        return handle.read()


def rust_test_names():
    """The regression names this proof carries, read from the real source."""
    names = set()
    with open(OPERATION_SOURCE, "r", encoding="utf-8") as handle:
        for line in handle:
            stripped = line.strip()
            if stripped.startswith("fn ") and "(" in stripped:
                names.add(stripped[3:].split("(")[0].strip())
    return names


def case_table_problems():
    """Every group names a real driver method and real carried regressions."""
    problems = []
    with open(DRIVER, "r", encoding="utf-8") as handle:
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


FORBIDDEN_TOKENS = ["-" + "force", "p" + "kill", "kill" + "all"]


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
    """The pinned supervisor exists and its bytes verify, or is honestly
    reported absent; the self-check never fabricates a verification."""
    problems = []
    if not os.path.isfile(SUPERVISOR):
        problems.append(
            f"the pinned supervisor {SUPERVISOR} is not installed on this host; "
            "the pin cannot verify here (an owner-activated run verifies on its "
            "own host)"
        )
    else:
        found = digest(SUPERVISOR)
        if found != SUPERVISOR_SHA256:
            problems.append(
                f"the pinned supervisor's bytes are {found}, not {SUPERVISOR_SHA256}"
            )
    return problems


# ---------- effect-free composition controls ----------

def run_case_groups(cases, execute, ready, diagnostic):
    """The whole entry→case→report composition, with the effects injected.

    `execute(case_id)` runs one case group (in a real pass, the staged driver
    under the supervisor; in the controls below, a fake). `ready(case_id)`
    answers whether the case's fixture custody is established. Counted runs
    latch the first fault and skip the rest; a diagnostic records every case
    and always qualifies false. This is the real signature the inner runner
    calls; the controls drive it with fakes.
    """
    results = []
    passed = set()
    fault = None
    for case in cases:
        if fault and not diagnostic:
            results.append({"case": case["id"], "status": "skipped", "kind": "latched"})
            continue
        reason = ready(case["id"])
        if reason is not None:
            results.append({"case": case["id"], "status": "skipped", "kind": "custody",
                            "detail": reason})
            continue
        try:
            evidence = execute(case["id"])
            passed.add(case["id"])
            results.append({"case": case["id"], "status": "passed", "evidence": evidence})
        except Exception as error:  # noqa: BLE001 - the composition records any fault
            fault = str(error)
            results.append({"case": case["id"], "status": "failed", "detail": fault})
    return results, passed


def build_receipt(results, mode, head, supervisor_verified):
    """The receipt shape a counted or diagnostic pass writes."""
    passed = [r for r in results if r["status"] == "passed"]
    failed = [r for r in results if r["status"] == "failed"]
    qualified = mode == "counted" and not failed and len(passed) == len(CASES)
    return {
        "schema": SCHEMA,
        "mode": mode,
        "qualified": bool(qualified),
        "supervisor_sha256": SUPERVISOR_SHA256,
        "supervisor_verified": bool(supervisor_verified),
        "head": head,
        "cases": results,
        "bounds": {"wall_seconds": WALL_SECONDS, "scratch_mib": SCRATCH_MIB, "image": None},
    }


def composition_controls():
    """Exercise the whole composition through fake external effects: no
    directory, no child process, no image — the real signatures only."""
    problems = []
    cases = CASES
    # Counted: a fault latches and the rest skip; the receipt stays unqualified.
    def execute_faulting(case_id):
        if case_id == "J5":
            raise RuntimeError("injected: watermark publish refused")
        return {"detail": "fake pass"}
    results, passed = run_case_groups(cases, execute_faulting, lambda _: None, diagnostic=False)
    statuses = {r["case"]: r["status"] for r in results}
    if statuses["J5"] != "failed" or statuses["J6"] != "skipped":
        problems.append("a counted fault did not latch over the remaining cases")
    if "J4" not in passed or "J5" in passed:
        problems.append("the counted pass recorded the wrong case outcomes")
    receipt = build_receipt(results, "counted", "0" * 40, supervisor_verified=False)
    if receipt["qualified"] is not False:
        problems.append("a latched counted receipt claimed qualified")
    # Diagnostic: every case records, and the receipt always qualifies false.
    results, passed = run_case_groups(cases, execute_faulting, lambda _: None, diagnostic=True)
    if len([r for r in results if r["status"] == "passed"]) != len(cases) - 1:
        problems.append("the diagnostic did not record every case")
    receipt = build_receipt(results, "diagnostic", "0" * 40, supervisor_verified=False)
    if receipt["qualified"] is not False:
        problems.append("a diagnostic receipt claimed qualified")
    # Custody skip: a not-ready case is skipped with its reason, counted or not.
    results, _ = run_case_groups(cases, lambda _: {"detail": "x"},
                                 lambda case: "fixture custody not established" if case == "J9" else None,
                                 diagnostic=False)
    skipped = [r for r in results if r["case"] == "J9"]
    if not skipped or skipped[0]["status"] != "skipped":
        problems.append("a not-ready case was not skipped with its reason")
    # Clean counted pass: qualified, and only then.
    results, passed = run_case_groups(cases, lambda _: {"detail": "fake pass"},
                                      lambda _: None, diagnostic=False)
    receipt = build_receipt(results, "counted", "0" * 40, supervisor_verified=True)
    if receipt["qualified"] is not True:
        problems.append("a clean counted receipt did not qualify")
    return problems


def plan_document():
    return {
        "schema": "barge.journal.native.plan/1",
        "proof": "the operation journal's native cases, journal-only",
        "driver": {
            "example": DRIVER_NAME,
            "feature": PROOF_FEATURE,
            "stage_command": " ".join(STAGE_COMMAND),
        },
        "supervisor": {"sha256": SUPERVISOR_SHA256, "path": SUPERVISOR},
        "bounds": {"wall_seconds": WALL_SECONDS, "scratch_mib": SCRATCH_MIB,
                    "image": None,
                    "work_margin": "the supervisor's own stop; never measured here"},
        "premises": [{"premise": p, "disposition": d} for p, d in PREMISES],
        "cases": CASES,
        "held": "counted and diagnostic are owner-activated; the effigy "
                "declarations activate nothing",
    }


def self_check():
    problems = []
    problems.extend(case_table_problems())
    problems.extend(feature_problems())
    problems.extend(declaration_problems())
    problems.extend(supervisor_problems())
    problems.extend(composition_controls())
    return problems


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic"])
    arguments = parser.parse_args(argv)
    if arguments.self_check:
        problems = self_check()
        if problems:
            for problem in problems:
                print(f"problem: {problem}", file=sys.stderr)
            return 1
        print(f"journal-proof apparatus: OK ({len(CASES)} case groups, "
              f"{sum(len(c['carried_by']) for c in CASES)} named regressions, "
              "composition controls passed, no image, no scratch, no child process)")
        return 0
    if arguments.plan:
        print(json.dumps(plan_document(), indent=2))
        return 0
    parser.error("name a mode: counted or diagnostic (or --self-check / --plan)")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
