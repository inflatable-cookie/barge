#!/usr/bin/env python3
"""Barge's Oxide outputs adapter proof: the delegated deletion on one owned image.

  python3 scripts/native/oxide-outputs-proof.py counted       # the author pass (owner-activated)
  python3 scripts/native/oxide-outputs-proof.py diagnostic    # the uncounted twin (owner-activated)
  python3 scripts/native/oxide-outputs-proof.py --self-check  # apparatus only, effect-free
  python3 scripts/native/oxide-outputs-proof.py --plan        # the plan, effect-free

It is owner-activated: no ordinary Effigy selector runs a counted or diagnostic
pass, and the declarations in `effigy.toml` activate nothing. This proof reuses
the established staging, custody and supervision helpers of
`scripts/native/release-proof.py` (imported, not copied): the same pinned
supervisor, the same verified Stash scratch route, the same guard containment
refusals, the same image-custody indication, ordinary-detach closure and receipt
discipline. One supervision/accounting framework exists, and this proof does not
write another.

What it does, in one sentence: after the qualified supervisor's bytes and this
host's Stash scratch route are verified, and under that supervisor (pinned
below, executed with the same interpreter), it creates one private run root
inside the guard's owned scratch, stages the `proof-synthetic-account` driver
build of HEAD inside the guard, attaches one owned APFS sparse image of 128 MiB
with ownership enabled, and runs twelve adapter cases through the staged
driver against a private HOME, a private state directory and a synthetic
account home. Each case is one delegated-deletion fact of the joint protocol
(Oxide `barge-adoption.md`, "Joint protocol for Barge output deletion"): the
authority the adapter reads, the identity it binds, the outcome it reports,
and the recovery it resumes.

What it never does:

  - it never stages or builds before the supervisor's bytes and the scratch
    route are verified, and staging spends the supervisor's 300 s wall;
  - it never touches the real account home or the real Barge state directory,
    and it never registers a real drive (Builds, Stash or Vault included);
  - it never deletes anything but synthetic roots inside its own image;
  - it never force-detaches; closure needs the ordinary detach's success and
    reap plus whole-backing attachment absence, so uncertain custody retains
    the image and faults the run;
  - it never writes outside the guard's owned scratch and its evidence
    directory.

The staged driver is the `proof-synthetic-account` feature build
(`crates/barge/examples/oxide_outputs_proof.rs`, `required-features`, so no
ordinary build compiles it). Under that feature alone, Oxide's reference
directories come from the synthetic account home. The ordinary production build
is a different binary and this proof does not run it; the receipt records that
as LIMITATION.

Modes:

  counted     Every case must pass. The first fault stops the remaining cases,
              a case whose dependency did not pass is skipped, and the receipt
              is `qualified: true` only on a clean pass.
  diagnostic  The uncounted twin: keep going after a fault, record every case,
              and always write `qualified: false`.

Receipt: $OXIDE_PROOF_EVIDENCE/oxide-outputs-proof-receipt.json. A missing
receipt is a missing proof.
"""

import argparse
import json
import os
import plistlib
import subprocess
import sys
import tempfile
import time
import traceback

SCRIPT_PATH = os.path.abspath(__file__)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(SCRIPT_PATH)))

import importlib.util  # noqa: E402
import types  # noqa: E402


def _import_release_proof():
    """Load the established proof helpers from `release-proof.py` (hyphenated)."""
    path = os.path.join(os.path.dirname(SCRIPT_PATH), "release-proof.py")
    spec = importlib.util.spec_from_file_location("barge_release_proof", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


base = types.SimpleNamespace() if False else _import_release_proof()

SCHEMA = "barge.oxide-outputs.native/1"
SELF_CHECK_SCHEMA = "barge.oxide-outputs.native.self-check/1"
RECEIPT_NAME = "oxide-outputs-proof-receipt.json"
DRIVER_NAME = "oxide_outputs_proof"
# One fresh 128 MiB owned image, as the brief proposes. Sparse, so the
# allocated size is far below the logical size.
IMAGE_MIB = 128
IMAGE_VOLUME_NAME = "barge-proof-outputs"
# The same envelope the qualified supervisor gives every elevated proof. This
# is an unmeasured proposal: source-readiness review judges the fit, and a
# concrete bound impediment is reported rather than silently enlarged.
LIMIT_SECONDS = base.LIMIT_SECONDS
SCRATCH_BUDGET_MIB = base.SCRATCH_BUDGET_MIB
# No case starts inside this window before the guard's deadline.
WORK_STOP_SECONDS = 30
PROOF_FEATURE = base.PROOF_FEATURE
TOOLCHAIN_CHANNEL = base.TOOLCHAIN_CHANNEL
RUN_ROOT_PREFIX = "outputs-proof-"
LIMITATION = (
    "The end-to-end receipt runs a proof-only build (feature "
    "`proof-synthetic-account`) whose Oxide reference directories come from one "
    "synthetic account home. It tests the adapter logic over synthetic "
    "AccountFacts. It does not establish that the production account lookup is "
    "equivalent, that a live drive or a live Oxide pool behaves identically, or "
    "that any adoption, install, declaration or grant write is qualified; none "
    "of those is authorised by this slice."
)

# The driver build runs inside the guard, through Oxide, locked and offline.
BUILD_COMMAND = [
    "oxide", "build", "-p", "barge", "--locked", "--offline",
    "--features", PROOF_FEATURE, "--example", DRIVER_NAME,
    "--message-format", "json",
]

# Each case is one adapter claim, carried by one named regression in
# `crates/barge/src/oxide_outputs/tests.rs` and exercised here end to end on
# the owned image. `carried_by` must name a real test; the self-check proves
# it. `method` names the driver case this script invokes.
CASES = [
    {"id": "A1", "method": "A1",
     "claim": "one covered root is deleted; the outcome binds the Barge intent "
              "id that named the quarantine, and the settled record names the "
              "caller",
     "carried_by": "a_covered_root_is_deleted_and_the_report_binds_the_barge_intent"},
    {"id": "A2", "method": "A2",
     "claim": "a withdrawn grant (a re-declaration that omits it) refuses "
              "before any effect and keeps the root",
     "carried_by": "an_undeclared_withdrawn_mismatched_or_stale_grant_is_refused_before_any_effect"},
    {"id": "A3", "method": "A3",
     "claim": "a request whose recorded grant generation is newer than the "
              "stored record refuses and names the generation",
     "carried_by": "an_undeclared_withdrawn_mismatched_or_stale_grant_is_refused_before_any_effect"},
    {"id": "A4", "method": "A4",
     "claim": "the legacy oxide.outputs/2 fence is not the covered class and "
              "refuses",
     "carried_by": "the_legacy_fence_is_not_covered_and_an_unknown_fence_is_unknown"},
    {"id": "A5", "method": "A5",
     "claim": "an unreadable Oxide registry is unknown, which keeps the root "
              "and keeps the caller pending",
     "carried_by": "unreadable_or_unknown_authority_is_unknown_and_keeps_everything"},
    {"id": "A6", "method": "A6",
     "claim": "a settled Oxide intent is refused; the adapter acts on a live "
              "intent only",
     "carried_by": "a_selected_mismatched_settled_or_absent_live_intent_is_refused"},
    {"id": "A7", "method": "A7",
     "claim": "marker bytes other than the ones the intent bound keep the root",
     "carried_by": "a_changed_marker_nonce_digest_or_owner_is_refused"},
    {"id": "A8", "method": "A8",
     "claim": "a generic host delete of an oxide_outputs unit refuses, and a "
              "generic replay settles nothing; the host grant deletes no Oxide "
              "root",
     "carried_by": "a_generic_host_release_never_deletes_or_settles_an_oxide_outputs_root"},
    {"id": "A9", "method": "A9",
     "claim": "a live hold on the covered root keeps it through custody",
     "carried_by": "a_live_hold_keeps_the_covered_root"},
    {"id": "A10", "method": "A10",
     "claim": "a pending intent whose root is still in place rolls back, the "
              "record is removed, and the lookup answers positively absent",
     "carried_by": "a_pre_rename_pending_intent_rolls_back_and_the_root_stays"},
    {"id": "A11", "method": "A11",
     "claim": "a post-rename pending intent resumes to completion, and the "
              "lookup answers found-terminal with the same id",
     "carried_by": "a_post_rename_pending_intent_resumes_to_completion"},
    {"id": "A12", "method": "A12",
     "claim": "this call's own Locked is the known-no-effect fact; with a "
              "pending intent under the same caller id it stays pending",
     "carried_by": "a_pre_intent_lock_refusal_is_known_no_effect_and_a_pending_intent_under_lock_is_pending"},
    {"id": "A13", "method": "A13",
     "claim": "an actual deleted record refuses a different request or Barge id "
              "as unknown for binding, preserves bytes, and recovers the exact request",
     "carried_by": "the_recovery_checks_pending_and_terminal_operation_bindings"},
]

# Spelled apart so this list does not contain the tokens it forbids.
FORBIDDEN_TOKENS = ["-" + "force", "p" + "kill", "kill" + "all"]


def source_text():
    with open(SCRIPT_PATH, "r", encoding="utf-8") as handle:
        return handle.read()


def rust_test_names():
    """The test names this proof names, read from the real Rust source.

    The suite's tests are `fn a_...()`, `fn an_...()`, `fn the_...()` and
    `fn unreadable_...()` declarations; a line scan is enough to prove a
    carried_by name is a real test.
    """
    path = os.path.join(REPO_ROOT, "crates", "barge", "src", "oxide_outputs", "tests.rs")
    names = set()
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            stripped = line.strip()
            if stripped.startswith("fn ") and "(" in stripped:
                name = stripped[3:].split("(")[0].strip()
                # The suite's test names begin with one of these articles or
                # the one long noun name; helper fns never do.
                if name.startswith(("a_", "an_", "the_", "unreadable_")):
                    names.add(name)
    return names


def case_table_problems():
    """Every case names a real driver method and a real carried regression."""
    problems = []
    text = source_text()
    methods = {part.split('"')[0] for part in text.split('"method": "')[1:]}
    names = rust_test_names()
    for case in CASES:
        if case["method"] not in methods:
            problems.append(f"case {case['id']} names no driver method this script runs")
        if case["carried_by"] not in names:
            problems.append(
                f"case {case['id']} carries no named regression "
                f"({case['carried_by']} is not a test in oxide_outputs/tests.rs)"
            )
    for token in FORBIDDEN_TOKENS:
        if token in text:
            problems.append(f"the proof script contains `{token}`")
    return problems


def feature_problems():
    """The driver target requires the proof feature; no default enables it."""
    import re
    problems = []
    manifest_path = os.path.join(REPO_ROOT, "crates", "barge", "Cargo.toml")
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = handle.read()
    if f'name = "{DRIVER_NAME}"' not in manifest:
        problems.append(f"the {DRIVER_NAME} example target is not declared")
    if 'required-features = ["proof-synthetic-account"]' not in manifest:
        problems.append("the driver example does not require the proof feature")
    if f"{PROOF_FEATURE} = []" not in manifest:
        problems.append(f"the proof feature {PROOF_FEATURE} is not declared")
    if re.search(r"^default\s*=", manifest, re.MULTILINE):
        problems.append("a default feature is declared, which would enable the proof build by default")
    return problems


def executed_shapes_problems():
    """The guard half's executed call shapes, read from this script's own source.

    The effect-free self-check must prove the shapes a native run executes, not
    only the pins: the attach census is backing-filtered before any mount is
    accepted, custody is live before the attach child runs, the detach census
    names this backing, the staged build runs under the stage environment with
    its refusal check before the build, and a case passes only on the driver's
    own `passed` verdict with a zero exit.
    """
    problems = []
    # The anchors carry each method's own docstring head, so this scan never
    # matches its own mentions of them.
    text = source_text()
    attach = text.split('def attach(self):\n        """Attach one owned 128 MiB image', 1)[1]
    attach = attach.split("\n    def detach", 1)[0]
    if "image_attachments(backing" not in attach:
        problems.append("the attach census is not filtered to this run's backing")
    if attach.find("self.attached = True") > attach.find("HDIUTIL"):
        problems.append("custody is not live before the attach child runs")
    if "-mountpoint" not in attach:
        problems.append("the attach does not name this run's own mount point")
    if "outputs-image.sparseimage" not in attach:
        problems.append("the backing is not named with the suffix hdiutil realizes for -type SPARSE")
    detach = text.split('def detach(self, purpose):\n        """Ordinary detach only', 1)[1]
    detach = detach.split("\n    def case_environment", 1)[0]
    if "image_attachments(self.backing" not in detach:
        problems.append("the detach census is not filtered to this run's backing")
    # The staging selector's body is the last definition of its own line in
    # this file, so rsplit takes the real function, never this scan's mention.
    selector = text.rsplit('def driver_from_build(text):', 1)[1]
    selector = selector.split("\n\ndef build_driver", 1)[0]
    if '"example" in (target.get("kind") or [])' not in selector:
        problems.append("the staging selector does not require the example target kind")
    if 'target.get("name") == DRIVER_NAME' not in selector:
        problems.append("the staging selector does not require the driver's target name")
    if 'and document.get("executable")' not in selector:
        problems.append("the staging selector accepts an artifact with no executable")
    build = text.rsplit('def build_driver(parent, deadline_ns):', 1)[1]
    build = build.split("\n\nclass AdapterProof", 1)[0]
    if "base.stage_environment(dict(os.environ), parent)" not in build:
        problems.append("the staged build does not run under the stage environment")
    if build.find("stage_environment_problems") > build.find("base.run(BUILD_COMMAND"):
        problems.append("the stage environment's refusal check does not run before the build")
    if "found = driver_from_build(result.stdout)" not in build:
        problems.append("the staged build is not resolved through the driver selector")
    if 'found.get("features") != [PROOF_FEATURE]' not in build:
        problems.append("the staged build's feature list is not checked")
    # The selection predicate is executed here over synthetic artifact lines,
    # so the shape it accepts is the shape cargo emits for this example target:
    # the `barge` bin line, a wrong target name and a missing executable are
    # all refused, and only the example's own artifact with its features is
    # selected.
    artifact = (
        '{"reason":"compiler-artifact","target":{"kind":["example"],'
        '"crate_types":["bin"],"name":"' + DRIVER_NAME + '","edition":"2021"},'
        '"features":["' + PROOF_FEATURE + '"],"executable":"/stage/' + DRIVER_NAME + '"}'
    )
    selected = driver_from_build(artifact)
    if not selected or selected.get("executable") != "/stage/" + DRIVER_NAME:
        problems.append("the staging selector does not select the driver's example artifact")
    if selected and selected.get("features") != [PROOF_FEATURE]:
        problems.append("the staging selector reports features other than the artifact's")
    bin_line = (
        '{"reason":"compiler-artifact","target":{"kind":["bin"],'
        '"crate_types":["bin"],"name":"barge","edition":"2021"},'
        '"features":["' + PROOF_FEATURE + '"],"executable":"/stage/barge"}'
    )
    if driver_from_build(bin_line) is not None:
        problems.append("the staging selector accepts the barge binary instead of the driver")
    wrong_name = (
        '{"reason":"compiler-artifact","target":{"kind":["example"],'
        '"crate_types":["bin"],"name":"another_example","edition":"2021"},'
        '"features":["' + PROOF_FEATURE + '"],"executable":"/stage/another_example"}'
    )
    if driver_from_build(wrong_name) is not None:
        problems.append("the staging selector accepts another target's artifact")
    no_executable = (
        '{"reason":"compiler-artifact","target":{"kind":["example"],'
        '"crate_types":["bin"],"name":"' + DRIVER_NAME + '","edition":"2021"},'
        '"features":["' + PROOF_FEATURE + '"]}'
    )
    if driver_from_build(no_executable) is not None:
        problems.append("the staging selector accepts an artifact with no executable")
    run_case = text.split('def run_case(self, case):\n        """One case, one driver process', 1)[1]
    run_case = run_case.split("\n    def receipt_document", 1)[0]
    if 'verdict.get("status") == "passed"' not in run_case or "result.returncode == 0" not in run_case:
        problems.append("a case passes on anything but the driver's own passed verdict")
    if run_case.find("base.stage_environment") == -1 or "**self.case_environment(case_dir)" not in run_case:
        problems.append("the case environment does not compose the scrub with the case bindings")
    elif run_case.find("base.stage_environment") > run_case.find("**self.case_environment(case_dir)"):
        problems.append("the case bindings are not applied after the stage scrub")
    # The composition is executed here: the scrub alone drops the guard hook
    # variables, and the composed environment a case child actually runs under
    # must still carry both bindings the proof feature's account seam requires
    # (crates/barge/src/os/proof_account.rs).
    composed = {
        **base.stage_environment(
            {
                "HOME": "/case/home",
                "TMPDIR": "/case",
                "TMP": "/case",
                "TEMP": "/case",
                "OXIDE_TASK_SCRATCH_ROOT": "/guard/scratch",
                "OXIDE_PROOF_EVIDENCE": "/guard/evidence",
            },
            "/case",
        ),
        **{
            "HOME": "/case/home",
            "BARGE_STATE_DIR": "/case/state",
            "BARGE_ACCOUNT_HOME": "/case/home",
            "TMPDIR": "/case",
            "TMP": "/case",
            "TEMP": "/case",
            "OXIDE_TASK_SCRATCH_ROOT": "/guard/scratch",
            "OXIDE_PROOF_EVIDENCE": "/guard/evidence",
        },
    }
    for binding in ("OXIDE_TASK_SCRATCH_ROOT", "OXIDE_PROOF_EVIDENCE", "BARGE_STATE_DIR", "BARGE_ACCOUNT_HOME"):
        if not composed.get(binding):
            problems.append(f"the case child loses the required binding {binding}")
    driver_path = os.path.join(REPO_ROOT, "crates", "barge", "examples", f"{DRIVER_NAME}.rs")
    with open(driver_path, encoding="utf-8") as handle:
        driver = handle.read()
    if '"passed"' not in driver:
        problems.append("the driver never prints a passed verdict for the script to parse")
    # D5: the fixture checkout must be a boot-volume, never-created path. A
    # checkout under the case home sits on a mount parent whose volume the
    # intent identity does not carry, so the reviewed F2 rule refuses it and
    # the covered positive deletion never runs.
    if 'CHECKOUT_PATH_PREFIX: &str = "/private/var/tmp/barge-proof-checkout-"' not in driver:
        problems.append("the fixture checkout is not a boot-volume, never-created path")
    if 'bindings.home.join("checkout")' in driver:
        problems.append("the fixture checkout still sits under the case home on a mount parent")
    # D6: the A12 holder must outlast the design lock wait, or the same-call
    # Locked paths are never exercised (the call acquires the lock instead).
    if "HOLD_LONGER_THAN_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(7)" not in driver:
        problems.append("the A12 holder does not hold longer than the design lock wait")
    # D7: A9 must discriminate custody's own live-hold fact from an earlier
    # refusal such as the checkout absence proof, which runs before custody.
    if '"custody is not proven"' not in driver or '"is live"' not in driver:
        problems.append("A9 does not discriminate custody's live-hold refusal")
    # The A12 readiness premise is observed, not slept on: the holder sends
    # its acknowledgement only after its lock is positively held, the case
    # waits bounded, both A12 branches gate the adapter call on that
    # observation, and no fixed readiness sleep remains before an adapter
    # call. Structural checks of the helper's contract; the diagnostic itself
    # proves the behavior.
    if "fn wait_for_lock_ready" not in driver:
        problems.append("the holder readiness acknowledgement helper is missing")
    if "recv_timeout" not in driver:
        problems.append("the readiness wait is not bounded")
    if "LOCK_READY_TIMEOUT" not in driver:
        problems.append("the readiness wait bound is not declared")
    if "from_millis(300)" in driver:
        problems.append("a fixed readiness sleep is still present before an adapter call")
    locked = driver.split("fn case_locked_outcomes(fixture: &Fixture)", 1)[-1].split(
        "\n/// How long the case waits", 1
    )[0]
    ready_sites = locked.count("wait_for_lock_ready(&ready_rx)")
    call_sites = locked.count("fixture.call(&intent")
    gated = locked.count("Ok(()) => fixture.call(&intent")
    if ready_sites != 2 or call_sites != 2 or gated != 2:
        problems.append(
            "both A12 branches must gate exactly one adapter call each behind the observed readiness"
        )
    first_ready = locked.find("wait_for_lock_ready(&ready_rx)")
    first_call = locked.find("fixture.call(&intent")
    last_ready = locked.rfind("wait_for_lock_ready(&ready_rx)")
    last_call = locked.rfind("fixture.call(&intent")
    if first_ready > first_call or last_ready > last_call:
        problems.append("an adapter call is not gated behind its observed readiness")
    return problems


def driver_problems():
    """The driver is safe code that refuses to run without its bindings."""
    problems = []
    driver_path = os.path.join(REPO_ROOT, "crates", "barge", "examples", f"{DRIVER_NAME}.rs")
    with open(driver_path, encoding="utf-8") as handle:
        driver = handle.read()
    for binding in ("BARGE_PROOF_VOLUME_UUID", "BARGE_PROOF_VOLUME_MOUNT", "BARGE_STATE_DIR"):
        if binding not in driver:
            problems.append(f"the driver never checks {binding}")
    if "unsafe {" in driver or "unsafe fn" in driver:
        problems.append("the driver carries its own unsafe block")
    return problems


def premise_dispositions():
    """The nine proof-authoring premises, traced to source and child environment.

    Each entry names the source that executes the premise and the environment
    the child runs under. `n/a` is explicit, never silent.
    """
    return {
        "private_home_rustup": (
            "every case child gets HOME, BARGE_ACCOUNT_HOME and BARGE_STATE_DIR "
            "inside the guard's owned scratch (case_environment here; the "
            "binding rules are base.fixture_environment's); the staged compile "
            "is the ordinary managed Oxide build with the real account's HOME, "
            f"CARGO_HOME and rustup home, pinned to RUSTUP_TOOLCHAIN "
            f"{TOOLCHAIN_CHANNEL} with auto-install disabled and the committed "
            "lock; no fixture child compiles anything"
        ),
        "account_home": (
            "the proof feature binds Oxide's reference directories to the "
            "validated synthetic home (crates/barge/src/os/proof_account.rs), "
            "which refuses an unbound run, an escape from the supervisor's "
            "scratch, and a private home that is not the account home; each "
            "case's Oxide records are written inside that home only"
        ),
        "environment": (
            "the guarded half runs under base.stage_environment's scrub of "
            "inherited OXIDE, XDG, BARGE and Rust-flag variables; each case "
            "child inherits only the bindings case_environment names, and the "
            "driver refuses a missing binding before any fixture is built"
        ),
        "adoption": (
            "n/a: nothing is adopted and no tool configuration is written; the "
            "fixture volume is registered directly into the private state "
            "directory and the area is declared explicitly"
        ),
        "cargo_argv": (
            "n/a: no fake Cargo external subcommand exists in this proof; the "
            "only child processes are the pinned supervisor, the managed Oxide "
            "build and the driver itself"
        ),
        "owned_image": (
            "one 128 MiB sparse APFS image attached -owners on, its backing "
            "inside the run root; base.custody_document publishes the guard's "
            "image-custody indication before the backing exists; the attach is "
            "accepted only through base.attach_document's binding (one mounted "
            "entity, this device, no second mount); detach is ordinary only "
            "and uncertain custody is retained, never repaired, retried or "
            "force-detached"
        ),
        "compiler_flags": (
            "the staged build runs under base.stage_environment's flag scrub "
            "(RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS, DYLD_INSERT_LIBRARIES "
            "removed; guard hooks dropped), pinned toolchain, "
            "RUSTUP_AUTO_INSTALL=0, CARGO_NET_OFFLINE=true, --locked --offline"
        ),
        "temporary_storage": (
            "the run root and every case directory are created by "
            "tempfile.mkdtemp with an explicit owned parent inside the guard's "
            "scratch; the image backing lives inside the run root; nothing is "
            "written to a drive root, a home folder or a fixed /tmp path"
        ),
        "mktemp": (
            "every temporary directory uses tempfile.mkdtemp with an explicit "
            "dir= the guard verified; no bare mktemp and no fixed name"
        ),
    }


def self_check():
    """The apparatus only: pins, the case table, the feature and the driver."""
    problems = []
    problems += base.installed_supervisor_problems()
    problems += base.argv_guard_problems()
    problems += case_table_problems()
    problems += feature_problems()
    problems += driver_problems()
    problems += executed_shapes_problems()
    if base.SUPERVISOR_SHA256 != "fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b":
        problems.append("the imported supervisor pin is not the qualified one")
    if IMAGE_MIB != 128:
        problems.append("this proof's image is not the brief's 128 MiB")
    if WORK_STOP_SECONDS != 30:
        problems.append("the work stop is not the brief's 30 s")
    return problems


def inner_command(mode):
    """The argv the supervisor runs: this script and its mode. No artifact is named."""
    return [sys.executable, SCRIPT_PATH, "--inner", mode]


def supervisor_command(supervisor, mode):
    return [
        sys.executable, supervisor,
        "--limit", str(LIMIT_SECONDS),
        "--scratch-budget-mib", str(SCRATCH_BUDGET_MIB),
        "--root-parent", base.DESIGNATED_SCRATCH,
        "--", *inner_command(mode),
    ]


def argv_guard_problems():
    """The outer command and the inner parser agree, and no artifact is named."""
    problems = []
    text = source_text()
    inner = text.split("def inner_half(", 1)
    if len(inner) != 2:
        problems.append("the guarded half is missing")
    else:
        body = inner[1].split("\ndef ", 1)[0]
        if "guard_refusals(" not in body or "guarded_run(" not in body:
            problems.append("the guarded half does not refuse its premises before it stages")
        elif body.find("guard_refusals(") > body.find("guarded_run("):
            problems.append("the guarded half stages before it refuses a non-guard caller")
    for mode in ("counted", "diagnostic"):
        if f'"{mode}"' not in text.split("def main(", 1)[-1]:
            problems.append(f"the inner parser does not accept {mode}")
    return problems


def plan_document():
    return {
        "schema": SELF_CHECK_SCHEMA,
        "status": "held: no owner activation; no receipt exists",
        "supervisor": {"name": base.SUPERVISOR_NAME, "sha256": base.SUPERVISOR_SHA256},
        "route": {
            "designated_scratch": base.DESIGNATED_SCRATCH,
            "volume_mount": base.SCRATCH_VOLUME_MOUNT,
            "volume_uuid": base.SCRATCH_VOLUME_UUID,
            "rule": "the same verified Stash scratch route the release proof "
                    "uses, verified before any staging or fixture effect",
        },
        "build": {
            "feature": PROOF_FEATURE,
            "command": BUILD_COMMAND,
            "staged_under": "this occurrence's guarded root, staged inside the "
                            "guard like the release proof stages its binary; no "
                            "shared stage directory and no unguarded prewarm",
            "stage_budget": f"inside the guard's {LIMIT_SECONDS} s wall; a "
                            "deadline reached before staging refuses, never resets a clock",
            "production_build": "not run by this proof",
            "toolchain": TOOLCHAIN_CHANNEL,
        },
        "isolation": {
            "fixture_bindings": [
                "HOME", "BARGE_ACCOUNT_HOME", "BARGE_STATE_DIR",
                "BARGE_PROOF_VOLUME_UUID", "BARGE_PROOF_VOLUME_MOUNT",
                "BARGE_PROOF_CASE_DIR", "OXIDE_TASK_SCRATCH_ROOT",
                "OXIDE_PROOF_EVIDENCE",
            ],
            "oxide_directories": {
                "state": base.OXIDE_STATE_RELATIVE,
                "cache": base.OXIDE_CACHE_RELATIVE,
            },
            "rule": "every fixture path resolves inside the supervisor's owned "
                    "scratch; the synthetic account home is validated by the "
                    "proof feature's own binding checks",
        },
        "custody": {
            "indication": base.CUSTODY_NAME,
            "version": base.CUSTODY_VERSION,
            "published": "before the backing exists and before any attach, "
                         "retained for the run",
            "final_detach": "ordinary detach only, authorized by a fresh rebind "
                            "census with the mounted volume's own UUID; "
                            "uncertain custody retains the image and faults the "
                            "run",
            "closure": "ordinary child success and reap plus whole-backing "
                       "attachment absence plus an absent mount path",
        },
        "limitation": LIMITATION,
        "premise_dispositions": premise_dispositions(),
        "bounds": {
            "admission": "heavy",
            "image_mib": IMAGE_MIB,
            "wall_seconds": LIMIT_SECONDS,
            "scratch_budget_mib": SCRATCH_BUDGET_MIB,
            "detach": "ordinary",
            "work_stop": f"{WORK_STOP_SECONDS} s before the guard's deadline, "
                         "so no case starts inside the stop window",
            "may_delete": "synthetic fixture roots inside the owned image only",
        },
        "commands": {
            "counted": supervisor_command("<supervisor>", "counted"),
            "diagnostic": supervisor_command("<supervisor>", "diagnostic"),
        },
        "activation_order": [
            "0. exact-head readiness review of the source and this apparatus",
            "1. the planner activates one uncounted diagnostic at the exact "
            "head; it proves nothing alone",
            "2. the planner activates one counted author pass at the same head",
            "3. a distinct retained reviewer activates its own counted pass at "
            "the same head",
            "4. the activator correlates this script's receipt with the "
            "supervisor's own receipt; a missing or unclear half is a missing "
            "proof",
        ],
        "cases": [{"id": case["id"], "claim": case["claim"],
                   "carried_by": case["carried_by"]} for case in CASES],
    }


def driver_from_build(text):
    """The staged driver's source and features from `oxide build --message-format json`.

    Cargo reports one `compiler-artifact` per target. This build produces an
    **example** target, not the `barge` binary, so release-proof's own selector
    (`base.executable_from_build`, which accepts only the `barge` bin) never
    matches it; this is the same strict shape check with the example target's
    name and kind. A line whose target is not this exact example, whose kind
    does not carry `example`, or whose executable is missing is not the
    driver, whatever else it says.
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
        if (
            document.get("reason") == "compiler-artifact"
            and target.get("name") == DRIVER_NAME
            and "example" in (target.get("kind") or [])
            and document.get("executable")
        ):
            return {"executable": document["executable"], "features": document.get("features", [])}
    return None


def build_driver(parent, deadline_ns):
    """Stage the proof-feature driver inside the guard; own bytes, own manifest.

    The build runs under base.stage_environment exactly as the release proof's
    staging does: the guard's hooks and every inherited Rust flag variable are
    dropped, the toolchain is pinned, auto-install is disabled and the network
    is offline, and the refusal check runs before any build starts. The whole
    stage spends only the wall the guard still holds, never a minimum that
    could run past it. The copy is one exclusive directory under the guard's
    owned root, hashed into its own manifest, and the feature list the
    compiler reports for the driver's own artifact must be exactly the proof
    feature.
    """
    remaining = None
    if deadline_ns is not None:
        remaining = (deadline_ns - time.monotonic_ns()) / 1e9
        if remaining <= 0:
            raise base.Infrastructure(
                "stage", "no guard wall is left to stage the driver build"
            )
    head = base.git_head()
    if not head:
        raise base.Infrastructure("stage", "HEAD is unreadable, so the staged build cannot be named")
    environment = base.stage_environment(dict(os.environ), parent)
    problems = base.stage_environment_problems(environment)
    if problems:
        raise base.Infrastructure("stage", "; ".join(problems))
    stage = tempfile.mkdtemp(prefix="proof-binary-", dir=parent)
    budget = None if remaining is None else int(remaining)
    result = base.run(BUILD_COMMAND, environment, cwd=REPO_ROOT, timeout=budget)
    found = driver_from_build(result.stdout)
    if found is None:
        raise base.Infrastructure(
            "stage",
            f"the build reported no {DRIVER_NAME} example artifact, so no driver can be staged",
        )
    if found.get("features") != [PROOF_FEATURE]:
        raise base.Infrastructure(
            "stage",
            f"the driver artifact was built with features {found.get('features')}, "
            f"not [{PROOF_FEATURE!r}]",
        )
    staged = os.path.join(stage, DRIVER_NAME)
    base.run(["cp", found["executable"], staged], environment)
    os.chmod(staged, 0o755)
    manifest = {
        "schema": base.STAGE_SCHEMA,
        "head": head,
        "sha256": base.sha256_file(staged),
        "bytes": os.path.getsize(staged),
        "toolchain": TOOLCHAIN_CHANNEL,
        "feature": PROOF_FEATURE,
        "build": BUILD_COMMAND,
        "source": found["executable"],
        "path": staged,
    }
    with open(os.path.join(stage, base.STAGE_MANIFEST), "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, indent=1)
        handle.write("\n")
    return {"path": staged, "manifest": manifest}


class AdapterProof:
    """One guarded run: attach one owned image, run the cases, close ordinarily."""

    def __init__(self, binary, receipt_path, diagnostic, evidence, route, parent, owned):
        self.binary = binary
        self.receipt_path = receipt_path
        self.diagnostic = diagnostic
        self.evidence = evidence
        self.route = route
        self.parent = parent
        self.owned = owned
        self.results = []
        self.first_fault = None
        self.attached = False
        self.unknown_custody = False
        self.mount = None
        self.device = None
        self.volume_uuid = None
        self.backing = None
        self.run_root = None
        self.custody = None
        self.closure = None
        self.infrastructure = None
        self.isolation = None
        self.resources = {}

    def attach(self):
        """Attach one owned 128 MiB image inside the run root, custody first.

        The backing's own path is canonical, the mount is this run's explicit
        name, and the custody indication is published before the backing
        exists. Custody is live the moment this process may have attached:
        `self.attached` is set before the attach child runs, so an interrupted
        or unreadable attach is retained unknown, never reported as "no
        attachment was established" and never cleaned up. The census is
        filtered to this backing, so another image mounted on the host can
        never be accepted, used or detached.
        """
        # The suffix is the one hdiutil realizes for -type SPARSE: a backing
        # named without it would be created as <name>.sparseimage while every
        # later step named the nonexistent <name>, which is exactly how the
        # first diagnostic lost its attach. Name the real file.
        backing = os.path.join(self.run_root, "outputs-image.sparseimage")
        if os.path.realpath(backing) != backing:
            raise base.Infrastructure("attach", "the backing path is not canonical")
        self.custody = base.custody_document(backing, self.run_root, self.evidence)
        with open(os.path.join(self.evidence, base.CUSTODY_NAME), "w", encoding="utf-8") as handle:
            json.dump(self.custody, handle, indent=1)
        mount = f"/Volumes/barge-outputs-proof-{os.getpid()}-{int(time.time()) % 100000}"
        if os.path.lexists(mount):
            raise base.Infrastructure("attach", "the mount name already exists")
        self.backing = backing
        self.mount = mount
        self.attached = True  # An interrupted attach has unknown custody, not absence.
        base.run(
            [base.HDIUTIL, "create", "-quiet", "-size", f"{IMAGE_MIB}m", "-fs", "APFS",
             "-type", "SPARSE", "-volname", IMAGE_VOLUME_NAME, "-o", backing],
            os.environ, timeout=120,
        )
        attach = base.run(
            [base.HDIUTIL, "attach", "-nobrowse", "-owners", "on", "-mountpoint", mount,
             "-plist", backing],
            os.environ, timeout=120, check=False,
        )
        # The census is this backing's own entities, never the host's list of
        # every mounted image: a foreign image cannot be accepted as ours.
        census = base.image_attachments(backing, os.environ)
        device = None
        if census is not None and attach.returncode == 0:
            mounted = [row for row in census if row[1] == mount]
            if len(mounted) == 1:
                device = mounted[0][0]
        facts = base.attach_document(attach.returncode, True, census, mount, device)
        if facts.get("state") != "attached":
            raise base.Infrastructure("attach", facts.get("reason", "unknown custody"))
        self.device = device
        info = base.run([base.DISKUTIL, "info", "-plist", mount], os.environ, timeout=base.INFO_TIMEOUT_SECONDS)
        parsed = plistlib.loads(info.stdout.encode())
        uuid = parsed.get(base.VOLUME_UUID_KEY)
        if not uuid:
            raise base.Infrastructure("attach", "the owned image did not report its volume UUID")
        self.volume_uuid = uuid
        text = base.run([base.DISKUTIL, "info", mount], os.environ, timeout=base.INFO_TIMEOUT_SECONDS).stdout
        if base.ownership_state(text) != "enabled":
            raise base.Infrastructure(
                "attach", f"the image does not state `Owners: Enabled` (reads {base.ownership_state(text)!r})"
            )

    def detach(self, purpose):
        """Ordinary detach only; closure needs reap plus whole-backing absence.

        The rebind census is this backing's own, read fresh; a foreign image's
        bindings never authorize this detach. An unreadable census or a binding
        mismatch retains the image as unknown custody and faults the run rather
        than detaching anything.
        """
        rows = base.image_attachments(self.backing, os.environ)
        if rows is None:
            raise base.Infrastructure("rebind-census", "the pre-detach census is unreadable")
        observed_uuid = base.volume_uuid(self.mount)
        problems = base.detach_binding_problems(
            rows, self.mount, self.device, self.volume_uuid, observed_uuid
        )
        if problems:
            raise base.Infrastructure("rebind-census", "; ".join(problems))
        detach = base.run(
            [base.HDIUTIL, "detach", self.mount], os.environ,
            timeout=base.DETACH_TIMEOUT_SECONDS, check=False,
        )
        rows_after = base.image_attachments(self.backing, os.environ)
        closure = base.closure_document(
            purpose, detach.returncode, True, rows_after,
            not os.path.lexists(self.mount),
        )
        closure["mount"] = self.mount
        return closure

    def case_environment(self, case_dir):
        """The bindings one case child runs under, all inside the owned scratch."""
        home = os.path.join(case_dir, "home")
        state = os.path.join(case_dir, "state")
        os.makedirs(home, exist_ok=True)
        os.makedirs(state, exist_ok=True)
        return {
            "HOME": home,
            "TMPDIR": case_dir,
            "TMP": case_dir,
            "TEMP": case_dir,
            "BARGE_STATE_DIR": state,
            "BARGE_ACCOUNT_HOME": home,
            "BARGE_PROOF_VOLUME_UUID": self.volume_uuid,
            "BARGE_PROOF_VOLUME_MOUNT": self.mount,
            "OXIDE_TASK_SCRATCH_ROOT": self.parent,
            "OXIDE_PROOF_EVIDENCE": self.evidence,
        }

    def run_case(self, case):
        """One case, one driver process, one verdict document."""
        case_dir = tempfile.mkdtemp(prefix=f"case-{case['id']}-", dir=self.run_root)
        # The stage scrub exists for the staged build child. A fixture case
        # child needs the guard's own bindings back on top of it, because the
        # proof feature's account seam refuses to resolve Oxide's directories
        # without OXIDE_TASK_SCRATCH_ROOT and OXIDE_PROOF_EVIDENCE. The scrub
        # runs first and the case bindings win, so the child keeps the toolchain
        # and flag hygiene of the scrub and still carries exactly the bindings
        # its fixture requires.
        scrubbed = base.stage_environment(
            {
                "HOME": os.path.join(case_dir, "home"),
                "TMPDIR": case_dir,
                "TMP": case_dir,
                "TEMP": case_dir,
            },
            case_dir,
        )
        environment = {**scrubbed, **self.case_environment(case_dir)}
        started = time.monotonic()
        result = base.run([self.binary, case["method"]], environment, timeout=60, check=False)
        elapsed = time.monotonic() - started
        verdict = {}
        for line in reversed(result.stdout.splitlines()):
            line = line.strip()
            if line.startswith("{"):
                try:
                    candidate = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if candidate.get("case") == case["method"]:
                    verdict = candidate
                    break
        passed = (
            result.returncode == 0
            and verdict.get("status") == "passed"
        )
        record = {
            "case": case["id"],
            "status": "passed" if passed else "failed",
            "seconds": round(elapsed, 3),
            "detail": str(verdict.get("detail") or result.stderr[-base.FAILURE_TEXT_CHARS:]),
            "exit": result.returncode,
        }
        if not passed and not self.diagnostic and self.first_fault is None:
            self.first_fault = case["id"]
        self.results.append(record)
        return passed

    def receipt_document(self, cancelled):
        passed = [record for record in self.results if record["status"] == "passed"]
        return {
            "schema": SCHEMA,
            "mode": "diagnostic" if self.diagnostic else "counted",
            "binary_sha256": base.sha256_file(self.binary),
            "route": self.route,
            "cases": self.results,
            "all_cases_passed": len(passed) == len(CASES),
            "first_fault": self.first_fault,
            "infrastructure": self.infrastructure,
            "image": {"mib": IMAGE_MIB, "mount": self.mount, "device": self.device},
            "custody": self.custody,
            "closure": self.closure,
            "isolation": self.isolation,
            "resources": self.resources,
            "cancelled": cancelled,
            "limitation": LIMITATION,
        }

    def write_receipt(self, receipt):
        with open(self.receipt_path, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=1)


def guarded_run(mode, receipt_path, route, deadline_ns, parent, owned):
    """Stage inside the guard, attach one owned image, run the cases, close."""
    evidence = os.path.dirname(os.path.abspath(receipt_path))
    try:
        build = build_driver(parent, deadline_ns)
    except base.Infrastructure as error:
        print(json.dumps({"schema": SCHEMA, "ok": False, "stage": None,
                          "refused": f"staging the driver build failed: {error.detail}"}))
        return 125
    proof = AdapterProof(build["path"], receipt_path, mode == "diagnostic",
                         evidence, route, parent, owned)
    proof.isolation = {"problems": case_table_problems() + feature_problems() + driver_problems()}
    try:
        if proof.isolation["problems"]:
            raise base.Fault("preflight", "; ".join(proof.isolation["problems"]))
        proof.run_root = tempfile.mkdtemp(prefix=RUN_ROOT_PREFIX, dir=parent)
        proof.attach()
        deadline = (deadline_ns / 1_000_000_000) if deadline_ns else None
        stop_before = (
            time.monotonic()
            if deadline is None
            else deadline - WORK_STOP_SECONDS
        )
        started = time.monotonic()
        for case in CASES:
            if time.monotonic() >= stop_before:
                proof.results.append({
                    "case": case["id"], "status": "unexecuted",
                    "detail": f"the run's stop window ({WORK_STOP_SECONDS} s) "
                              "was reached before this case",
                })
                continue
            if proof.first_fault is not None and not proof.diagnostic:
                proof.results.append({
                    "case": case["id"], "status": "skipped",
                    "detail": f"the first fault ({proof.first_fault}) stopped the cases",
                })
                continue
            proof.run_case(case)
        proof.resources["run_seconds"] = round(time.monotonic() - started, 3)
    except base.Infrastructure as error:
        proof.unknown_custody = proof.attached
        proof.infrastructure = {"check": error.check, "detail": error.detail}
    except base.Fault as error:
        proof.unknown_custody = proof.attached
        proof.results.append({"case": "setup", "status": "failed",
                              "kind": error.check, "detail": error.detail})
        proof.first_fault = proof.first_fault or "setup"
    except Exception as error:  # noqa: BLE001 - the receipt must exist
        proof.unknown_custody = True
        proof.results.append({"case": "setup", "status": "failed", "kind": "exception",
                              "detail": f"{type(error).__name__}: {error}",
                              "trace": traceback.format_exc()[-800:]})

    # Closure: ordinary detach only; uncertain custody retains and faults.
    try:
        if proof.unknown_custody:
            proof.closure = {"purpose": "final", "closed": False, "custody": "retained",
                             "mount": proof.mount,
                             "detail": "unknown custody; retained without repair, "
                                       "retry or force detach"}
        elif proof.attached:
            proof.closure = proof.detach("final")
        else:
            proof.closure = {"purpose": "final", "closed": False, "custody": "retained",
                             "mount": proof.mount,
                             "detail": "no attachment was established in this run"}
    except Exception as error:  # noqa: BLE001
        proof.closure = {"purpose": "final", "closed": False, "custody": "retained",
                         "mount": proof.mount,
                         "detail": f"the detach raised: {type(error).__name__}: {error}"}

    recorded = {result["case"] for result in proof.results}
    for case in CASES:
        if case["id"] not in recorded:
            proof.results.append({"case": case["id"], "status": "unexecuted",
                                  "detail": "setup or infrastructure did not complete"})
    receipt = proof.receipt_document(None)
    qualified = (
        mode == "counted"
        and proof.infrastructure is None
        and receipt["all_cases_passed"]
        and isinstance(proof.closure, dict)
        and proof.closure.get("closed") is True
        and proof.closure.get("backing_attached") is False
        and proof.isolation is not None
        and not proof.isolation["problems"]
    )
    receipt["qualified"] = bool(qualified)
    receipt["classification"] = (
        "infrastructure" if proof.infrastructure is not None
        else "qualified" if qualified else "unqualified"
    )
    receipt["receipt_path"] = receipt_path
    proof.write_receipt(receipt)
    print(json.dumps({
        "schema": receipt["schema"],
        "qualified": receipt["qualified"],
        "classification": receipt["classification"],
        "all_cases_passed": receipt["all_cases_passed"],
        "first_fault": receipt["first_fault"],
        "closure": receipt["closure"],
        "receipt_path": receipt_path,
        "cases": [{"case": r["case"], "status": r["status"]} for r in proof.results],
    }, indent=1))
    return 0


def inner_half(mode, receipt_path):
    """Refuse anything but the pinned supervisor's guarded child, then prove."""
    evidence = os.environ.get("OXIDE_PROOF_EVIDENCE")
    scratch = os.environ.get("OXIDE_TASK_SCRATCH_ROOT", "")
    tmpdir = os.environ.get("TMPDIR", "")
    containment = base.guard_containment_facts(scratch, evidence or "")
    facts = {
        "evidence_isdir": bool(evidence) and os.path.isdir(evidence),
        "scratch_isdir": bool(scratch) and os.path.isdir(scratch),
        "tmpdir_inside_scratch": base.inside_owned(tmpdir, scratch),
        "containment_problems": base.guard_containment_problems(containment),
        "supervisor_problems": base.installed_supervisor_problems(),
        "route": carried_route(),
        "deadline_ns": wall_deadline(),
    }
    refusals = base.guard_refusals(facts)
    if refusals:
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "; ".join(refusals)}))
        return 125
    if receipt_path is None:
        receipt_path = os.path.join(evidence, RECEIPT_NAME)
    owned_root = base.guard_owned_root(scratch)
    return guarded_run(mode, receipt_path, facts["route"], facts["deadline_ns"],
                       owned_root, {"owned root": owned_root, "evidence directory": evidence})


def carried_route():
    """The scratch route the outer half verified, or None when it carried none."""
    raw = os.environ.get(base.ROUTE_ENV)
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return None


def wall_deadline():
    """The absolute monotonic deadline the outer half launched the guard with."""
    raw = os.environ.get(base.WALL_DEADLINE_ENV)
    try:
        value = int(raw)
    except (TypeError, ValueError):
        return None
    return value if value > 0 else None


def outer_half(mode):
    """Verify the pinned supervisor and this host's scratch route, then hand over."""
    supervisor = base.supervisor_path()
    problems = base.installed_supervisor_problems()
    if problems:
        print(json.dumps({"schema": SCHEMA, "ok": False, "refused": "; ".join(problems)}))
        return 125
    facts = base.scratch_route_facts()
    route_problems = base.scratch_route_problems(facts)
    if route_problems:
        print(json.dumps({"schema": SCHEMA, "ok": False,
                          "refused": "; ".join(route_problems),
                          "route": base.route_document(facts)}))
        return 125
    env = dict(os.environ)
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env.pop(base.ROUTE_ENV, None)
    env[base.ROUTE_ENV] = json.dumps(base.route_document(facts), separators=(",", ":"))
    env[base.WALL_DEADLINE_ENV] = str(time.monotonic_ns() + (LIMIT_SECONDS - 2) * 1_000_000_000)
    env["BARGE_PROOF_SUPERVISOR"] = supervisor
    result = subprocess.run(supervisor_command(supervisor, mode), env=env)
    return result.returncode


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", nargs="?", choices=["counted", "diagnostic"])
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--inner", choices=["counted", "diagnostic"])
    parser.add_argument("--receipt")
    args = parser.parse_args(argv)
    if args.self_check:
        problems = self_check()
        for problem in problems:
            print(f"oxide-outputs-proof self-check: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"oxide-outputs-proof self-check: {len(CASES)} cases carried by named "
              "regressions; the pinned supervisor, the scratch route, the 128 MiB "
              "image route and the closure gate hold")
        return 0
    if args.plan:
        json.dump(plan_document(), sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0
    if args.inner:
        return inner_half(args.inner, args.receipt)
    if args.mode is None:
        parser.error("name a mode: counted or diagnostic (or --self-check / --plan)")
    return outer_half(args.mode)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
