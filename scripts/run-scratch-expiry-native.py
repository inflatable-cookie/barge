#!/usr/bin/env python3
"""Run Barge's scratch expiry native proof under Oxide's qualified supervisor.

Barge writes no second supervisor. This wrapper checks the pinned supervisor's
bytes, then hands the fixed case script to it as the whole command, with the
design's 300 s wall clock and 512 MiB monitored scratch. Nothing here creates an
image, a session or an accounting harness of its own.

Usage:
  python3 scripts/run-scratch-expiry-native.py [--diagnostic] [--receipt <path>]

Without `--receipt` the case script writes its receipt into the supervisor's
evidence directory (`OXIDE_PROOF_EVIDENCE`) and prints that path; the receipt
never lands outside the guard's own root.

The owner activates each run separately: one uncounted `--diagnostic` pass, then
the counted pass at the exact head. A counted pass with a missing receipt is a
missing proof. The case script's own preflight runs first, inside this
supervisor, and it is the product's census, not Python's: one probe root in the
guard's image is really deleted by `barge scratch expire`, so the custody scan
that decides it is the Barge binary's own same-uid scan with the product's bounds
and barge#012's bounded EPERM re-observation. A product custody refusal
classifies the run `infrastructure` and aborts unqualified (exit 4) before any
case. The live pull case force-detaches only its rebound owned whole disk; all
other detaches are ordinary, and an uncertain detach leaves the proof unqualified.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

SCHEMA = "barge.scratch.expiry.native.launch/1"
SUPERVISOR_SHA256 = "bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924"
WALL_LIMIT_S = 300
SCRATCH_MIB = 512
# Barge's designated scratch, when the Builds drive is attached (AGENTS.md
# guardrails). The supervisor creates its own owned root below it.
DESIGNATED_SCRATCH = "/Volumes/Builds/scratch/barge"


def refuse(reason):
    print(json.dumps({"schema": SCHEMA, "ok": False, "refused": reason}))
    return 2


def main(argv):
    parser = argparse.ArgumentParser(description="run Barge's scratch native proof")
    parser.add_argument("--diagnostic", action="store_true")
    parser.add_argument("--receipt", default=None)
    options = parser.parse_args(argv)
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    case_script = os.path.join(repo, "scripts", "test-scratch-expiry.py")
    supervisor = os.environ.get(
        "BARGE_PROOF_SUPERVISOR", os.path.expanduser("~/Dev/projects/oxide/scripts/bounded-test.py")
    )
    if not os.path.isfile(supervisor):
        return refuse(f"the qualified supervisor is not at {supervisor}")
    digest = hashlib.sha256()
    with open(supervisor, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    if digest.hexdigest() != SUPERVISOR_SHA256:
        return refuse(
            f"{supervisor} is sha256 {digest.hexdigest()}, not the qualified {SUPERVISOR_SHA256}"
        )
    binary = os.environ.get("BARGE_PROOF_BIN")
    if not binary or not os.access(binary, os.X_OK):
        return refuse(
            "BARGE_PROOF_BIN must name an executable barge binary; build it through oxide "
            "and pass its path when activating the run"
        )
    command = [
        sys.executable, supervisor,
        "--limit", str(WALL_LIMIT_S),
        "--scratch-budget-mib", str(SCRATCH_MIB),
    ]
    if not os.path.isdir(DESIGNATED_SCRATCH):
        return refuse(f"the designated scratch parent is not present: {DESIGNATED_SCRATCH}")
    command += ["--root-parent", DESIGNATED_SCRATCH]
    command += ["--", sys.executable, case_script]
    if options.receipt:
        # An explicit receipt path is only honoured when it is inside the guard's
        # own evidence directory; otherwise the case script writes its receipt
        # beside the supervisor's evidence, never outside the guard's scratch.
        command += ["--receipt", options.receipt]
    if options.diagnostic:
        command.append("--diagnostic")
    env = dict(os.environ)
    # The native owns its root under the explicitly named parent, never a
    # caller's adopted scratch root or an inherited injection environment.
    env.pop("OXIDE_TASK_SCRATCH_ROOT", None)
    env.pop("DYLD_INSERT_LIBRARIES", None)
    env.pop("BARGE_PROOF_ENOTEMPTY_PATH", None)
    env.pop("BARGE_PROOF_ENOTEMPTY_LOG", None)
    env.pop("BARGE_PROOF_ENOTEMPTY_COUNT", None)
    env.pop("BARGE_PROOF_PAUSE_PATH", None)
    env.pop("BARGE_PROOF_PAUSE_LOG", None)
    env["BARGE_PROOF_BIN"] = binary
    result = subprocess.run(command, env=env)
    return result.returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
