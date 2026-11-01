#!/usr/bin/env python3
"""Barge F6-R1's native proof: the apparatus self-check and the plan (effect-free).

  python3 scripts/native/release-proof.py --self-check
  python3 scripts/native/release-proof.py --plan

This file is the declaration side of the proof only. The elevated run it would
supervise (one owned APFS fixture image, the qualified shared supervisor, a
diagnostic and a counted author pass, a distinct reviewer pass) is NOT built in
this change. Until its body lands and an owner activates it at an exact head,
there is no receipt, and a missing receipt is a missing proof.

--self-check checks the case table against the tests that carry each case, so a
renamed or deleted regression fails here. It starts no image, no scratch and no
child process. --plan prints the same table and the activation order as JSON and
changes nothing.
"""

import argparse
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
# The regressions a case may name: the library's release tests and the CLI contract.
TEST_SOURCES = [
    ROOT / "crates" / "barge" / "src" / "release.rs",
    ROOT / "crates" / "barge-cli" / "tests" / "cli_contract.rs",
]
EFFIGY = ROOT / "effigy.toml"

# The elevated run's bounds, as the F6-R1 brief fixes them. They are declared here
# for the plan; nothing enforces them until the native body exists.
BOUNDS = {
    "admission": "heavy",
    "image_max_mib": 64,
    "wall_seconds": 300,
    "scratch_max_mib": 512,
    "detach": "ordinary",
    "may_delete": "synthetic fixture roots only",
}

# Each case names the regression that carries it. The name must exist in one of
# TEST_SOURCES; the self-check reads those files to prove it.
CASES = [
    ("C1", "volume verified and attached", "an_absent_volume_plans_nothing_and_a_permanent_one_is_loud"),
    ("C2", "identity verified; record present", "all_six_conditions_release_a_root_and_each_one_alone_keeps_it"),
    ("C2e", "marker and inode re-proved under the unit lock", "the_unit_lock_re_proof_refuses_each_condition_it_re_reads"),
    ("C3", "custody safe; live hold keeps", "the_unit_lock_re_proof_refuses_each_condition_it_re_reads"),
    ("C4", "policy permits; age passed; release-by-reference has no age gate", "a_release_by_reference_policy_releases_a_root_without_an_age_gate"),
    ("C5", "no current reference from a complete set", "an_oxide_reference_that_changes_after_the_plan_keeps_the_root_or_the_batch"),
    ("C5b", "pending or unsettled Oxide reference keeps", "an_oxide_reference_that_is_not_settled_keeps_the_root"),
    ("C5c", "incomplete or divergent reference set keeps the batch", "an_incomplete_or_divergent_reference_source_keeps_the_whole_batch"),
    ("C6", "consumer path absent; a reappeared path keeps", "an_oxide_reference_that_changes_after_the_plan_keeps_the_root_or_the_batch"),
    ("X1", "one synthetic root released from the presented bytes", "a_release_run_deletes_one_root_the_presented_plan_allows_and_replaying_it_releases_nothing"),
    ("X2", "changed digest and non-canonical bytes refuse", "a_changed_digest_a_non_canonical_plan_or_another_area_refuses_and_changes_nothing"),
    ("X3", "foreign or unknown marker kind cannot qualify", "a_marker_with_an_unknown_kind_cannot_qualify_its_root"),
    ("X4", "each condition that changed after the plan keeps the root", "a_run_keeps_a_root_whose_condition_changed_after_the_plan_was_written"),
    ("X5", "intent written before the rename; replay after a remount", "an_intent_is_written_before_the_rename_and_a_remount_replays_it"),
    ("X6", "CLI refuses a digest that is not the presented bytes'", "prune_run_refuses_a_plan_whose_digest_or_bytes_are_not_the_presented_ones"),
    ("X7", "a named root waives only the age gate, and the plan says so", "a_named_root_waives_only_the_age_gate_and_the_plan_says_so"),
    ("X7b", "a named root must be listed; a waiver must name its root", "a_named_root_must_be_listed_and_a_waiver_must_name_its_root"),
    ("X8", "identity re-decided under the unit lock against the record's contents", "the_unit_lock_re_proof_refuses_each_condition_it_re_reads"),
]

# The apparatus's own refusal list: names that must not appear as activated
# native tasks while the body is unbuilt.
FORBIDDEN_TASKS = ["release:native", "release:native:diagnostic"]

ACTIVATION_ORDER = [
    "1. the native body lands and passes its own --self-check at the exact head",
    "2. the owner activates one uncounted diagnostic at that head; it proves nothing alone",
    "3. the owner activates one counted author pass at the same head",
    "4. a distinct reviewer activates its own counted pass at the same head",
]


def test_names():
    """Every `fn name(` in the case sources, as a set."""
    names = set()
    for source in TEST_SOURCES:
        text = source.read_text(encoding="utf-8")
        names.update(re.findall(r"fn ([a-z_0-9]+)\(", text))
    return names


def self_check():
    problems = []
    names = test_names()
    for case_id, _, test in CASES:
        if test not in names:
            problems.append(f"case {case_id} names `{test}`, which no case source defines")
    if len(CASES) != len({case_id for case_id, _, _ in CASES}):
        problems.append("two cases share an id")
    conditions = {case_id for case_id, _, _ in CASES if case_id[0] == "C"}
    for condition in ("C2", "C3", "C4", "C5", "C6"):
        if condition not in conditions:
            problems.append(f"no case covers condition {condition}")
    tasks = EFFIGY.read_text(encoding="utf-8")
    for task in FORBIDDEN_TASKS:
        if f'"{task}"' in tasks or f'[tasks."{task}"]' in tasks:
            problems.append(f"effigy.toml declares `{task}` before its native body exists")
    return problems


def plan():
    return {
        "proof": "barge F6-R1 release effect",
        "status": "held: native body not built; no receipt exists",
        "bounds": BOUNDS,
        "cases": [
            {"id": case_id, "claim": claim, "carried_by": test}
            for case_id, claim, test in CASES
        ],
        "activation_order": ACTIVATION_ORDER,
    }


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-check", action="store_true")
    mode.add_argument("--plan", action="store_true")
    args = parser.parse_args(argv)
    if args.self_check:
        problems = self_check()
        for problem in problems:
            print(f"release-proof self-check: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"release-proof self-check: {len(CASES)} cases carried by named regressions")
        return 0
    json.dump(plan(), sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
