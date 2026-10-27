#!/bin/bash
# Barge S6's native proof: the fixed command, in its two modes.
#
#   scripts/native/s6-cache-placement-proof.sh counted
#   scripts/native/s6-cache-placement-proof.sh diagnostic
#
# This script is the *whole command* the elevated native proof runs, and both
# Effigy selectors (`cache:native`, `cache:native:diagnostic`) are exactly these
# two lines after the CLI is built from the same head:
#
#   oxide build -p barge-cli --locked
#   scripts/native/s6-cache-placement-proof.sh counted|diagnostic
#
# It is owner-activated (docs/knowledge/contracts/s6-native-proof.md). It is not
# a test: nothing in the ordinary Effigy selectors runs it, it is not part of
# `effigy test`, and a run below the elevated tier must not invoke it.
#
# What it does, in one sentence: under Oxide's qualified shared supervisor
# (scripts/bounded-test.py, bytes pinned below, run with /usr/bin/python3), it
# creates one private run root
# inside the guard's owned scratch, attaches one owned APFS sparse image of at
# most 128 MiB with ownership enabled, registers that image as a fixture volume
# in a private state directory, and runs the S6 cache cases through the real
# `barge` CLI against a private account home — then writes its receipt into the
# supervisor's evidence directory.
#
# What it never does:
#  - it never touches the real account home, the real Barge state directory, the
#    real `~/.npmrc`, `~/.bunfig.toml` or `~/.cargo/`;
#  - it never writes to a registered real drive (Builds included);
#  - it never deletes cache data: undo removes a link or a marker, never a root;
#  - it never force-detaches anything but its own image's whole-disk device;
#  - it never runs without the supervisor's contract environment.
#
# Modes:
#  counted     Every case must pass; the first fault stops the remaining cases
#              and the receipt is `qualified: true` only on a clean pass.
#  diagnostic  The uncounted twin: keep going after a fault, latch the first
#              fault, skip a case whose dependency did not pass, and always
#              write a `qualified: false` receipt. It proves nothing on its own.
#
# Receipt: $OXIDE_PROOF_EVIDENCE/s6-cache-placement-receipt.json, plus one
# compact JSON summary line on stdout. A missing receipt is a missing proof.

set -uo pipefail

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
readonly SCRIPT_PATH
REPO_ROOT="$(cd "$(dirname "${SCRIPT_PATH}")/../.." && pwd)"
readonly REPO_ROOT

# Premise 9: the qualified shared supervisor, at the bytes this plan was written
# against, and no second supervisor. A different file is refused, never run.
#
# The supervisor is a Python file, not an executable: Oxide keeps it mode 0644
# and runs it with python3. Its readability and its digest are the gate; the
# execute bit is not, and this script never changes the file's mode.
readonly SUPERVISOR_PATH_DEFAULT="${HOME}/Dev/projects/oxide/scripts/bounded-test.py"
readonly SUPERVISOR_SHA256="bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924"

# The elevated tier's fixed resources.
readonly LIMIT_SECONDS=300
readonly SCRATCH_BUDGET_MIB=512
readonly IMAGE_MIB=128

# The per-case preflight's sentinel bound. The sentinel is a child of this run in
# the shared supervisor's own session; the probe observes it while it is alive,
# then terminates and reaps it inside this same bound, before the case starts.
readonly PREFLIGHT_SENTINEL_BOUND_SECONDS=2

readonly MODE="${1:-}"

usage() {
  printf 'usage: %s counted|diagnostic [<barge binary>]\n' "$(basename "$SCRIPT_PATH")" >&2
}

if [[ "$MODE" != "counted" && "$MODE" != "diagnostic" ]]; then
  usage
  exit 2
fi

readonly BARGE_BIN_INPUT="${2:-${BARGE_S6_BINARY:-${REPO_ROOT}/target/debug/barge}}"
readonly SUPERVISOR="${BARGE_S6_SUPERVISOR:-$SUPERVISOR_PATH_DEFAULT}"

# ---- the outer half: this must be the supervisor's own run -------------------

if [[ -z "${OXIDE_PROOF_EVIDENCE:-}" ]]; then
  # A caller outside the guard is either the owner activating a proof or a
  # mistake. Both get the same answer: only the pinned supervisor may run this.
  # `bounded-test.py` is a Python file kept mode 0644, so a readable regular file
  # is the whole precondition; its identity is the digest checked next.
  if [[ ! -f "$SUPERVISOR" || ! -r "$SUPERVISOR" ]]; then
    printf 's6: the qualified supervisor %s is not a readable regular file; this proof runs only under it\n' \
      "$SUPERVISOR" >&2
    exit 125
  fi
  actual_supervisor_sha="$(shasum -a 256 "$SUPERVISOR" | awk '{print $1}')"
  if [[ "$actual_supervisor_sha" != "$SUPERVISOR_SHA256" ]]; then
    printf 's6: %s is %s, not the pinned %s; refusing to run a different supervisor\n' \
      "$SUPERVISOR" "$actual_supervisor_sha" "$SUPERVISOR_SHA256" >&2
    exit 125
  fi
  if [[ ! -f "$BARGE_BIN_INPUT" || ! -x "$BARGE_BIN_INPUT" ]]; then
    printf 's6: %s is not an executable CLI; build it first with `oxide build -p barge-cli --locked`\n' \
      "$BARGE_BIN_INPUT" >&2
    exit 125
  fi
  # The pinned bytes are executed by the interpreter, never through their mode.
  exec /usr/bin/python3 "$SUPERVISOR" \
    --limit "$LIMIT_SECONDS" \
    --scratch-budget-mib "$SCRATCH_BUDGET_MIB" \
    -- "$SCRIPT_PATH" "$MODE" "$BARGE_BIN_INPUT"
fi

# ---- the inner half: the guarded run ----------------------------------------

readonly EVIDENCE="$OXIDE_PROOF_EVIDENCE"
readonly RECEIPT="$EVIDENCE/s6-cache-placement-receipt.json"
readonly RECEIPT_HOME="$EVIDENCE/receipt-home"
# Every case's full log is retained beside the receipt, so a failed case is
# identified by its CLI error document long after the guard's scratch is gone.
readonly CASE_LOGS="$EVIDENCE/cases"

refuse() {
  printf 's6: %s\n' "$*" >&2
  exit 125
}

[[ -n "${OXIDE_TASK_SCRATCH_ROOT:-}" ]] ||
  refuse "OXIDE_TASK_SCRATCH_ROOT is unset; this is not the supervisor's owned scratch"
[[ -n "${TMPDIR:-}" ]] || refuse "TMPDIR is unset; this is not the supervisor's owned scratch"
case "$TMPDIR" in
  "$OXIDE_TASK_SCRATCH_ROOT"*) ;;
  *) refuse "TMPDIR $TMPDIR is not inside the guard's owned scratch $OXIDE_TASK_SCRATCH_ROOT" ;;
esac
[[ -d "$EVIDENCE" ]] || refuse "the evidence directory $EVIDENCE is not there"
BARGE_BIN="$(cd "$(dirname "$BARGE_BIN_INPUT")" && pwd)/$(basename "$BARGE_BIN_INPUT")"
[[ -x "$BARGE_BIN" ]] || refuse "$BARGE_BIN is not an executable CLI"
BARGE_BIN_SHA256="$(shasum -a 256 "$BARGE_BIN" | awk '{print $1}')"
[[ "$BARGE_BIN_SHA256" =~ ^[0-9a-f]{64}$ ]] || refuse "cannot hash the Barge executable"
readonly BARGE_BIN_SHA256

# The shared supervisor starts this run as one guarded leader in its own
# session. That session is captured once and every case's preflight requires it
# to be unchanged, so a re-parented or re-sessioned run refuses before a case.
PREFLIGHT_SESSION="$(/bin/ps -o sess= -p $$ | tr -d ' ')"
[[ -n "$PREFLIGHT_SESSION" ]] || refuse "cannot read this run's session id"
readonly PREFLIGHT_SESSION
PREFLIGHT_RUNS=0
PREFLIGHT_FAILURE=""
PREFLIGHT_LAST_SENTINEL=""
PREFLIGHT_LAST_CENSUS=""

STARTED_MS="$(/usr/bin/python3 -c 'import time;print(int(time.time()*1000))')"
readonly STARTED_MS
RUN="$(mktemp -d "$TMPDIR/barge-s6-run.XXXXXX")" || refuse "cannot create the run root"
readonly RUN
readonly CASES_TSV="$EVIDENCE/cases.tsv"
: >"$CASES_TSV"
# One bounded row per preflight probe, one preflight per case that runs.
readonly PREFLIGHT_TSV="$EVIDENCE/preflight.tsv"
: >"$PREFLIGHT_TSV"
# One row per (premise, case body, concrete assertion): the design's nine-premise
# checklist traced through the case bodies, folded into the receipt against the
# cases that actually ran.
readonly PREMISE_TRACE_TSV="$EVIDENCE/premise-trace.tsv"
: >"$PREMISE_TRACE_TSV"

# Premise 3: every inherited scratch/spool pointer moves inside the run root.
export HOME="$RUN/home"
export CARGO_HOME="$RUN/cargo-home"
export BARGE_STATE_DIR="$RUN/state"
export BARGE_ACCOUNT_HOME="$RUN/home"
export TMP="$TMPDIR"
export TEMP="$TMPDIR"
unset CARGO_TARGET_DIR || true
mkdir -p "$HOME" "$CARGO_HOME" "$BARGE_STATE_DIR" "$RUN/backing" "$RUN/cases" "$CASE_LOGS"
mkdir -m 700 "$RECEIPT_HOME" || refuse "cannot create the private receipt runtime home"

# Premise 2: the real account state, read once before and once after, so the
# receipt proves it was not touched. It is only ever read.
REAL_ACCOUNT_HOME="$(dscl . -read "/Users/$(id -un)" NFSHomeDirectory 2>/dev/null | awk '{print $2}')"
[[ -n "$REAL_ACCOUNT_HOME" ]] || REAL_ACCOUNT_HOME="/Users/$(id -un)"
readonly REAL_STATE="$REAL_ACCOUNT_HOME/Library/Application Support/barge/state"
case "$REAL_STATE" in
  "$RUN"*) refuse "the real account state $REAL_STATE resolved inside the run root" ;;
esac

digest_tree() {
  # A stable digest of one tree's names, kinds, sizes, mtimes and file contents.
  local path="$1"
  if [[ ! -e "$path" ]]; then
    printf 'absent\n'
    return 0
  fi
  (
    cd "$path" || exit 1
    find . -exec stat -f 'S %N %HT %z %m' {} \; 2>/dev/null | LC_ALL=C sort
    find . -type f -print0 2>/dev/null | xargs -0 shasum -a 256 2>/dev/null | LC_ALL=C sort
  ) | shasum -a 256 | awk '{print $1}'
}

# >>> s6-adopted-tree-digest
digest_tree_excluding() {
  # Marker publication changes the containing directory's mtime and size. Keep
  # its identity and mode, every payload entry and byte, and every link's text;
  # exclude only the root marker and the root's expected metadata changes.
  /usr/bin/python3 - "$1" "$2" <<'PY'
import hashlib, json, os, stat, sys

root, excluded = sys.argv[1:]
rows = []

def visit(relative):
    path = os.path.join(root, relative)
    facts = os.lstat(path)
    row = [relative, facts.st_dev, facts.st_ino, facts.st_mode]
    if relative != ".":
        row.extend([facts.st_size, facts.st_mtime_ns])
    if stat.S_ISLNK(facts.st_mode):
        row.append(os.readlink(path))
    elif stat.S_ISREG(facts.st_mode):
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(65536), b""):
                digest.update(chunk)
        row.append(digest.hexdigest())
    elif not stat.S_ISDIR(facts.st_mode):
        raise ValueError("unexpected payload kind: " + path)
    rows.append(row)
    if stat.S_ISDIR(facts.st_mode):
        for name in sorted(os.listdir(path)):
            if relative == "." and name == excluded:
                continue
            visit(os.path.join(relative, name))

visit(".")
print(hashlib.sha256(json.dumps(rows, separators=(",", ":")).encode()).hexdigest())
PY
}
# <<< s6-adopted-tree-digest

REAL_STATE_BEFORE="$(digest_tree "$REAL_STATE")"
readonly REAL_STATE_BEFORE
REAL_STATE_AFTER="$REAL_STATE_BEFORE"

# ---- the owned image --------------------------------------------------------

VOLNAME="BargeProof$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
readonly VOLNAME
readonly MOUNT="/Volumes/$VOLNAME"
readonly BACKING="$RUN/backing/$VOLNAME.sparseimage"
ATTACHED=0
DEVICE=""
KEEP_BACKING=0
ORDINARY_DETACH_FAILED=0
LAST_DETACH="none"
DETACH_ATTEMPTS=0
CLEANUP_DISPOSITION="pending"
readonly DETACH_TSV="$EVIDENCE/detach-attempts.tsv"
: >"$DETACH_TSV"

attach_image() {
  local output
  ATTACHED=1 # A failed attach may have partially attached; cleanup must establish detach.
  output="$(hdiutil attach -owners on -nobrowse -mountpoint "$MOUNT" "$BACKING" 2>&1)" || {
    KEEP_BACKING=1
    printf '%s\n' "$output"
    return 1
  }
  DEVICE="$(printf '%s\n' "$output" | awk 'END{print $1}')"
  ATTACHED=1
  [[ -d "$MOUNT" ]] || {
    printf 'the image did not mount at %s\n' "$MOUNT"
    return 1
  }
  return 0
}

# >>> s6-detach
record_detach() {
  # context, method, target, bound attachment device, whole disk, exit, output log.
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$1" "$2" "$3" "$DEVICE" "$4" "$5" "$6" >>"$DETACH_TSV"
}

detach_image() {
  local context="$1" status=0 whole="" log prior_keep="$KEEP_BACKING"
  LAST_DETACH="none"
  [[ "$ATTACHED" == 1 ]] || return 0
  # Do not retry a spent ordinary detach. Only cleanup may use a force fallback.
  if [[ "$ORDINARY_DETACH_FAILED" == 0 ]]; then
    DETACH_ATTEMPTS=$((DETACH_ATTEMPTS + 1))
    log="$EVIDENCE/detach-$DETACH_ATTEMPTS.log"
    ORDINARY_DETACH_FAILED=1
    KEEP_BACKING=1
    record_detach "$context" ordinary "$MOUNT" "" pending "$log" || { KEEP_BACKING=1; return 1; }
    hdiutil detach "$MOUNT" >"$log" 2>&1 || status=$?
    if ! record_detach "$context" ordinary "$MOUNT" "" "$status" "$log"; then
      KEEP_BACKING=1
      return 1
    fi
    if [[ "$status" == 0 ]]; then
      ATTACHED=0
      ORDINARY_DETACH_FAILED=0
      KEEP_BACKING="$prior_keep"
      LAST_DETACH="ordinary"
      return 0
    fi
    ORDINARY_DETACH_FAILED=1
    KEEP_BACKING=1
  fi
  [[ "$context" == cleanup ]] || return 1
  # Bind the fallback to the whole disk of the device from our own attach.
  # Failure or uncertainty keeps the backing even if later safe cleanup works.
  DETACH_ATTEMPTS=$((DETACH_ATTEMPTS + 1))
  log="$EVIDENCE/detach-$DETACH_ATTEMPTS.log"
  status=0
  record_detach "$context" force-binding "$DEVICE" "" pending "$log" || return 1
  diskutil info "$DEVICE" >"$log" 2>&1 || status=$?
  if [[ "$status" == 0 ]]; then
    whole="$(awk -F': *' '/Part of Whole/{print $2}' "$log")"
  fi
  if [[ ! "$whole" =~ ^disk[0-9]+$ ]] ||
      [[ ! "$DEVICE" =~ ^/dev/${whole}(s[0-9]+)*$ ]]; then
    [[ "$status" != 0 ]] || status=125
    record_detach "$context" force-binding "$DEVICE" "$whole" "$status" "$log" || true
    return 1
  fi
  record_detach "$context" force-binding "$DEVICE" "$whole" "$status" "$log" || return 1
  DETACH_ATTEMPTS=$((DETACH_ATTEMPTS + 1))
  log="$EVIDENCE/detach-$DETACH_ATTEMPTS.log"
  status=0
  record_detach "$context" force "/dev/$whole" "$whole" pending "$log" || return 1
  hdiutil detach "/dev/$whole" -force >"$log" 2>&1 || status=$?
  record_detach "$context" force "/dev/$whole" "$whole" "$status" "$log" || return 1
  [[ "$status" == 0 ]] || return 1
  ATTACHED=0
  LAST_DETACH="force"
  return 0
}
# <<< s6-detach

# ---- the receipt ------------------------------------------------------------

# >>> s6-runtime-receipt
receipt_python() (
  # Runtime startup must not recreate the removed fixture HOME. Ignore Python
  # startup hooks and disable bytecode; keep any OS runtime cache in evidence.
  export HOME="$RECEIPT_HOME"
  exec /usr/bin/python3 -I -B -S "$@"
)
# <<< s6-runtime-receipt

write_receipt() {
  # >>> s6-receipt
  local exit_status="$1"
  S6_RUN="$RUN" \
  S6_DETACH_TSV="$DETACH_TSV" \
  S6_CLEANUP_DISPOSITION="$CLEANUP_DISPOSITION" \
  S6_ATTACHED="$ATTACHED" \
  S6_KEEP_BACKING="$KEEP_BACKING" \
  S6_MODE="$MODE" \
  S6_EXIT="$exit_status" \
  S6_RECEIPT="$RECEIPT" \
  S6_EVIDENCE="$EVIDENCE" \
  S6_CASES="$CASES_TSV" \
  S6_PREMISE_TRACE="$PREMISE_TRACE_TSV" \
  S6_PREFLIGHT_TSV="$PREFLIGHT_TSV" \
  S6_PREFLIGHT_RUNS="$PREFLIGHT_RUNS" \
  S6_PREFLIGHT_FAILURE="$PREFLIGHT_FAILURE" \
  S6_PREFLIGHT_SESSION="$PREFLIGHT_SESSION" \
  S6_PREFLIGHT_BOUND="$PREFLIGHT_SENTINEL_BOUND_SECONDS" \
  S6_CASE_LOGS="$CASE_LOGS" \
  S6_HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || printf 'unknown')" \
  S6_REPO="$REPO_ROOT" \
  S6_BINARY="$BARGE_BIN" \
  S6_BINARY_SHA256="$BARGE_BIN_SHA256" \
  S6_SUPERVISOR="$SUPERVISOR" \
  S6_SUPERVISOR_SHA="$SUPERVISOR_SHA256" \
  S6_VOLUME="$VOLNAME" \
  S6_MOUNT="$MOUNT" \
  S6_BACKING="$BACKING" \
  S6_SCRATCH="$OXIDE_TASK_SCRATCH_ROOT" \
  S6_TMPDIR="$TMPDIR" \
  S6_HOME="$HOME" \
  S6_CARGO_HOME="$CARGO_HOME" \
  S6_STATE="$BARGE_STATE_DIR" \
  S6_REAL_STATE="$REAL_STATE" \
  S6_REAL_BEFORE="$REAL_STATE_BEFORE" \
  S6_REAL_AFTER="$REAL_STATE_AFTER" \
  S6_STARTED="$STARTED_MS" \
  S6_IMAGE_MIB="$IMAGE_MIB" \
  S6_LIMIT="$LIMIT_SECONDS" \
  S6_SCRATCH_MIB="$SCRATCH_BUDGET_MIB" \
  receipt_python - <<'PY'
import json, os, stat, sys, time

mode = os.environ["S6_MODE"]
cases = []
path = os.environ["S6_CASES"]
if os.path.exists(path):
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3:
                log = os.path.join(os.environ["S6_CASE_LOGS"], parts[0] + ".log")
                premises = []
                if len(parts) >= 4:
                    premises = [int(part) for part in parts[3].split(",") if part.strip().isdigit()]
                cases.append({
                    "id": parts[0],
                    "status": parts[1],
                    "evidence": parts[2],
                    "log": log if os.path.exists(log) else None,
                    "premises": premises,
                })

passed = sum(1 for case in cases if case["status"] == "pass")
failed = sum(1 for case in cases if case["status"] == "fail")
skipped = sum(1 for case in cases if case["status"] == "skipped")
first_fault = next((case["id"] for case in cases if case["status"] == "fail"), None)

premises = [
    {
        "id": 1,
        "premise": "A private HOME, CARGO_HOME and state directory, so no real account state is read or written.",
        "trace": ["guarded startup export", "receipt: home/cargo_home/state/account_home"],
        "evidence": {
            "home": os.environ["S6_HOME"],
            "cargo_home": os.environ["S6_CARGO_HOME"],
            "state": os.environ["S6_STATE"],
            "account_home": os.environ["S6_HOME"],
        },
    },
    {
        "id": 2,
        "premise": "The account home used for state resolution is the fixture's, and the real account state is untouched (before/after digest of the real state directory).",
        "trace": ["guarded startup export", "receipt: real_state before/after digest"],
        "evidence": {
            "account_home": os.environ["S6_HOME"],
            "environment_home": os.environ["S6_HOME"],
            "real_state": os.environ["S6_REAL_STATE"],
            "before": os.environ["S6_REAL_BEFORE"],
            "after": os.environ["S6_REAL_AFTER"],
        },
    },
    {
        "id": 3,
        "premise": "Environment isolation: BARGE_STATE_DIR, TMPDIR/TMP/TEMP and every inherited scratch point inside the guard's owned scratch; no inherited real paths.",
        "trace": ["guarded startup assertion"],
        "evidence": {
            "scratch": os.environ["S6_SCRATCH"],
            "tmpdir": os.environ["S6_TMPDIR"],
            "state": os.environ["S6_STATE"],
            "account_home_override": os.environ["S6_HOME"],
        },
    },
    {
        "id": 4,
        "premise": "Explicit adoption only: every fixture placement is adopted with an explicit command and a recorded plan, never by an implicit default.",
        "trace": ["every adopt in the case bodies is an explicit command"],
        "evidence": {"plan_case": "plan-effect-free", "explicit_flag": "--adopt-existing-root"},
    },
    {
        "id": 5,
        "premise": "Owned image with ownership enabled (-owners on), ordinary detach, never a forced detach of anything but the owned image's own whole-disk device.",
        "trace": ["startup attach", "cleanup detach"],
        "evidence": {
            "detach": "ordinary; forced only for this image's own whole disk, backing kept on failure",
            "volume": os.environ["S6_VOLUME"],
            "mount": os.environ["S6_MOUNT"],
            "backing": os.environ["S6_BACKING"],
            "image_mib": int(os.environ["S6_IMAGE_MIB"]),
            "owners": True,
        },
    },
    {
        "id": 6,
        "premise": "Every flag and environment hook used is listed with its purpose; no debug-only product bypass exists.",
        "trace": ["receipt: flags_and_hooks"],
        "evidence": {
            "receipt_runtime": {
                "home": os.path.join(os.environ["S6_EVIDENCE"], "receipt-home"),
                "interpreter_flags": {"-I": "ignore inherited Python paths and startup hooks",
                                      "-B": "disable bytecode writes",
                                      "-S": "disable site startup"},
            },
            "hooks": [
                {"name": "OXIDE_TASK_SCRATCH_ROOT", "purpose": "the supervisor's owned scratch; every fixture lives inside it"},
                {"name": "TMPDIR/TMP/TEMP", "purpose": "the guard's owned scratch, inherited from the supervisor"},
                {"name": "OXIDE_PROOF_EVIDENCE", "purpose": "where this receipt is written"},
                {"name": "BARGE_STATE_DIR", "purpose": "the product's documented fixture-only state directory"},
                {"name": "BARGE_ACCOUNT_HOME", "purpose": "the product's documented fixture-only account home; a normal run leaves it unset"},
                {"name": "HOME/CARGO_HOME", "purpose": "private case home/cargo home; receipt runtime HOME is isolated in evidence"},
                {"name": "BARGE_S6_SUPERVISOR", "purpose": "the pinned supervisor path (this script's input, not a product hook)"},
                {"name": "BARGE_S6_BINARY", "purpose": "the CLI built from the same head (this script's input, not a product hook)"},
            ],
            "product_flags": ["--json", "--kind", "--volume", "--placement", "--write-setting", "--adopt-existing-root", "--plan"],
            "debug_bypass": False,
        },
    },
    {
        "id": 7,
        "premise": "Temporary storage: everything inside the guard's one owned root; no home folder, no drive root, no fixed /tmp/<name>.",
        "trace": ["mktemp run root", "cleanup"],
        "evidence": {"scratch": os.environ["S6_SCRATCH"], "run_root": os.path.dirname(os.environ["S6_STATE"])},
    },
    {
        "id": 8,
        "premise": "mktemp -d under that root for every fixture directory, never a shared name.",
        "trace": ["run_case creates cases/<id>/{home,state,out}"],
        "evidence": {"run_root_pattern": "barge-s6-run.XXXXXX", "case_dirs": "cases/<case-id>/{home,state,out}"},
    },
    {
        "id": 9,
        "premise": "The qualified runner bytes and selector are named and hash-pinned; Barge adds no second supervisor, session or accounting harness.",
        "trace": ["startup supervisor pin", "per-case preflight: sentinel and full same-uid descriptor census", "effigy selector cache:native / cache:native:diagnostic"],
        "evidence": {
            "supervisor": os.environ["S6_SUPERVISOR"],
            "supervisor_sha256": os.environ["S6_SUPERVISOR_SHA"],
            "limit_seconds": int(os.environ["S6_LIMIT"]),
            "scratch_budget_mib": int(os.environ["S6_SCRATCH_MIB"]),
            "second_supervisor": False,
            "preflight_probes": ["supervisor", "sentinel-descriptors", "full-census"],
            "descriptor_primitive": "same Barge product observer: kinfo effective uid/start, listings, vnode paths and bounded re-observation",
        },
    },
]

real_after = os.environ["S6_REAL_AFTER"]
unchanged = real_after == os.environ["S6_REAL_BEFORE"]

# The design's nine-premise checklist, folded against the case results that
# actually ran: a premise is traced by the case bodies that assert it, and every
# mapped case must have passed for it to count.
premise_rows = []
premise_path = os.environ.get("S6_PREMISE_TRACE")
if premise_path and os.path.exists(premise_path):
    with open(premise_path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3 and parts[0].strip().isdigit():
                premise_rows.append((int(parts[0]), parts[1], parts[2][:400]))

case_by_id = {case["id"]: case for case in cases}
for premise in premises:
    observations = []
    for number, case_id, asserts in premise_rows:
        if number != premise["id"]:
            continue
        case = case_by_id.get(case_id)
        observations.append({
            "case": case_id,
            "asserts": asserts,
            "status": case["status"] if case else "not_run",
            "evidence": case["evidence"] if case else None,
        })
    premise["observations"] = observations
    premise["covered"] = bool(observations) and all(
        item["status"] == "pass" for item in observations
    )
    premise["missing"] = [item["case"] for item in observations if item["status"] != "pass"]

premise_trace = {
    "premises": len(premises),
    "covered": sum(1 for premise in premises if premise["covered"]),
    "missing": [premise["id"] for premise in premises if not premise["covered"]],
    "complete": len(premises) == 9 and all(premise["covered"] for premise in premises),
}

preflight_failure = os.environ.get("S6_PREFLIGHT_FAILURE") or None
preflight_observations = []
censuses = []
preflight_path = os.environ.get("S6_PREFLIGHT_TSV")
if preflight_path and os.path.exists(preflight_path):
    with open(preflight_path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3:
                if parts[1] == "full-census":
                    try:
                        census = json.loads(parts[2])
                        if not isinstance(census, dict) or "status" not in census:
                            raise ValueError("invalid census result")
                    except ValueError:
                        census = {"status": "infrastructure", "problem": "missing or malformed census result"}
                    censuses.append({"case": parts[0], **census})
                preflight_observations.append({
                    "case": parts[0],
                    "probe": parts[1],
                    "detail": parts[2][:400],
                })
preflight = {
    "probes": ["supervisor", "sentinel-descriptors", "full-census"],
    "primitive": "same Barge binary: product ProcessObserver::scan_open_descriptors (listing and vnode paths)",
    "censuses": censuses,
    "census_bounds": {"table": 8192, "processes": 4096, "descriptors": 65536,
                      "descriptors_per_process": 4096, "seconds": 10},
    "session": os.environ["S6_PREFLIGHT_SESSION"],
    "sentinel_bound_seconds": int(os.environ["S6_PREFLIGHT_BOUND"]),
    "runs": int(os.environ["S6_PREFLIGHT_RUNS"]),
    "status": "infrastructure" if preflight_failure is not None else "passed",
    "failure": preflight_failure,
    "aborted": preflight_failure is not None,
    "observations": preflight_observations,
}

all_cases = {"absent-path", "empty-directory", "setting-unchanged", "setting-written",
             "diverged-undo", "non-empty-refused", "hand-made-in-place",
             "plan-effect-free", "ordinary-detach"}
seen = {case["id"] for case in cases}
not_run = sorted(all_cases - seen)

summary = {
    "cases": len(all_cases),
    "recorded": len(cases),
    "passed": passed,
    "failed": failed,
    "skipped": skipped,
    "not_run": not_run,
    "first_fault": first_fault,
    "real_state_unchanged": unchanged,
}

detach_by_log = {}
with open(os.environ["S6_DETACH_TSV"], encoding="utf-8") as handle:
    for line in handle:
        context, method, target, device, whole, status, log = line.rstrip("\n").split("\t")
        exit_status = None if status == "pending" else int(status)
        detach_by_log[log] = {"context": context, "method": method, "target": target,
                              "bound_device": device, "whole_disk": whole or None,
                              "exit_status": exit_status, "succeeded": exit_status == 0,
                              "log": log}
detach_attempts = list(detach_by_log.values())
detach = {
    "attempts": detach_attempts,
    "ordinary_case_succeeded": any(row["context"] == "case" and row["method"] == "ordinary"
                                   and row["succeeded"] for row in detach_attempts),
    "force_attempted": any(row["method"] == "force" for row in detach_attempts),
    "cleanup_disposition": os.environ["S6_CLEANUP_DISPOSITION"],
    "attached": os.environ["S6_ATTACHED"] != "0",
    "retention_required": os.environ["S6_KEEP_BACKING"] != "0",
    "backing_present": os.path.lexists(os.environ["S6_BACKING"]),
}
detach_complete = (
    detach["ordinary_case_succeeded"] and not detach["force_attempted"]
    and not detach["attached"] and not detach["retention_required"]
    and detach["cleanup_disposition"] == "removed"
    and any(row["context"] == "cleanup" and row["method"] == "ordinary"
            and row["succeeded"] for row in detach_attempts)
    and all(row["method"] == "ordinary" and row["succeeded"] for row in detach_attempts)
)

qualified = (
    detach_complete
    and mode == "counted"
    and failed == 0
    and skipped == 0
    and not_run == []
    and unchanged
    and preflight_failure is None
    and {item["case"] for item in censuses if item["status"] == "passed"} == all_cases
    and len(censuses) == len(cases)
    and premise_trace["complete"]
    and int(os.environ["S6_PREFLIGHT_RUNS"]) == len(cases)
    and int(os.environ["S6_EXIT"]) == 0
)

receipt = {
    "schema": "barge.s6.native/1",
    "qualified": qualified,
    "mode": mode,
    "head": os.environ["S6_HEAD"],
    "repo": os.environ["S6_REPO"],
    "binary": os.environ["S6_BINARY"],
    "binary_sha256": os.environ["S6_BINARY_SHA256"],
    "supervisor": os.environ["S6_SUPERVISOR"],
    "supervisor_sha256": os.environ["S6_SUPERVISOR_SHA"],
    "evidence": os.environ["S6_EVIDENCE"],
    "volume": {
        "name": os.environ["S6_VOLUME"],
        "mount": os.environ["S6_MOUNT"],
        "backing": os.environ["S6_BACKING"],
        "size_mib": int(os.environ["S6_IMAGE_MIB"]),
        "owners": True,
    },
    "detach": detach,
    "real_state": {
        "path": os.environ["S6_REAL_STATE"],
        "before": os.environ["S6_REAL_BEFORE"],
        "after": real_after,
        "unchanged": unchanged,
    },
    "preflight": preflight,
    "premises": premises,
    "premise_trace": premise_trace,
    "cases": cases,
    "summary": summary,
    "started_ms": int(os.environ["S6_STARTED"]),
    "finished_ms": int(time.time() * 1000),
    "exit_status": int(os.environ["S6_EXIT"]),
}

def write_document():
    with open(os.environ["S6_RECEIPT"], "w", encoding="utf-8") as handle:
        json.dump(receipt, handle, indent=1, sort_keys=False)
        handle.write("\n")


def nofollow_absence(path):
    # Walk from / by directory descriptors; a symlink or unreadable ancestor is
    # uncertainty, never evidence that a dangling target is absent.
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        parts = [part for part in path.split("/") if part]
        if not path.startswith("/") or not parts or any(p in (".", "..") for p in parts):
            raise ValueError("expected absolute owned path")
        for index, part in enumerate(parts):
            try:
                facts = os.stat(part, dir_fd=fd, follow_symlinks=False)
            except FileNotFoundError:
                return {"path": path, "status": "absent"}
            if index == len(parts) - 1:
                return {"path": path, "status": "present", "mode": facts.st_mode}
            if not stat.S_ISDIR(facts.st_mode):
                return {"path": path, "status": "unknown", "problem": "non-directory ancestor"}
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
    except (OSError, ValueError) as error:
        return {"path": path, "status": "unknown", "problem": str(error)}
    finally:
        os.close(fd)


def verify_cleanup():
    observations = [nofollow_absence(os.environ[key]) for key in ("S6_RUN", "S6_BACKING")]
    absent = all(item["status"] == "absent" for item in observations)
    checks = receipt.setdefault("cleanup_verification", [])
    checks.append({"after_receipt_write": True, "absent": absent, "observations": observations})
    complete = all(check["absent"] for check in checks)
    receipt["detach_complete"] = detach_complete and complete
    receipt["qualified"] = qualified and complete
    if os.environ["S6_CLEANUP_DISPOSITION"] == "removed" and not complete:
        detach["cleanup_disposition"] = "post_receipt_remainder"
        detach["retention_required"] = True
        receipt["exit_status"] = int(os.environ["S6_EXIT"]) or 1
    return absent


# Publish only an unqualified provisional receipt until writer startup and the
# first write have finished. A clean final publication is checked again; failure
# is latched and written without deleting/retrying any remainder.
receipt["qualified"] = False
receipt["detach_complete"] = False
receipt["detach"]["cleanup_disposition"] = (
    "pending_verification" if os.environ["S6_CLEANUP_DISPOSITION"] == "removed"
    else os.environ["S6_CLEANUP_DISPOSITION"]
)
write_document()
absent = verify_cleanup()
if absent and os.environ["S6_CLEANUP_DISPOSITION"] == "removed":
    detach["cleanup_disposition"] = "removed"
write_document()
final_absent = verify_cleanup()
if not final_absent or not absent:
    # Never promote a failed first observation after a later state change.
    receipt["qualified"] = False
    receipt["detach_complete"] = False
    write_document()

print(json.dumps({"schema": receipt["schema"], "qualified": receipt["qualified"], "mode": mode,
                  "head": receipt["head"], "summary": summary,
                  "receipt": os.environ["S6_RECEIPT"]}, separators=(",", ":")))
if os.environ["S6_CLEANUP_DISPOSITION"] == "removed" and not (absent and final_absent):
    raise SystemExit(1)

PY
  # <<< s6-receipt
}

# >>> s6-cleanup
cleanup() {
  local status=$?
  detach_image cleanup || status=1
  REAL_STATE_AFTER="$(digest_tree "$REAL_STATE")"
  if [[ "$KEEP_BACKING" == 0 && "$ATTACHED" == 0 ]]; then
    if rm -rf "$RUN"; then
      CLEANUP_DISPOSITION="removed"
    else
      CLEANUP_DISPOSITION="remove_failed"
      KEEP_BACKING=1
      status=1
    fi
  else
    CLEANUP_DISPOSITION="kept"
    status=1
    printf 's6: kept the run root %s (image %s)\n' "$RUN" "$BACKING" >&2
  fi
  write_receipt "$status" || { printf 's6: receipt publication or cleanup verification failed\n' >&2; status=1; }
  exit "$status"
}
# <<< s6-cleanup
trap cleanup EXIT

# ---- fixtures ---------------------------------------------------------------

NOW_MS="$(/usr/bin/python3 -c 'import time;print(int(time.time()*1000))')"

write_registry() {
  # One fixture volume, registered in a private state directory. Its `root_dir`
  # is case-scoped, so a case's caches folder is observable on its own.
  local state_dir="$1" root_dir="$2"
  mkdir -p "$state_dir"
  cat >"$state_dir/registry.json" <<JSON
{"schema":"barge.registry/1","volumes":[{"volume_uuid":"${UUID:?}","mount_hint":"${MOUNT}","availability":"permanent","durability":"expendable","root_dir":"${root_dir}","folders":{"scratch":null,"caches":null},"origin":"declared","scratch_grants":{"delegations":[],"claims":[]},"generation":1,"registered_ms":${NOW_MS}}]}
JSON
}

# One CLI call whose document goes to `$1`; on a non-zero exit the document, its
# stderr and the invocation are echoed into the case log, so the retained log and
# the receipt identify the failure instead of recording an empty line.
barge_cli() {
  local out="$1"
  shift
  local status=0
  if [[ "$(shasum -a 256 "$BARGE_BIN" | awk '{print $1}')" != "$BARGE_BIN_SHA256" ]]; then
    printf 'the Barge binary changed after guarded startup; refusing invocation\n' >&2
    return 125
  fi
  "$BARGE_BIN" "$@" >"$out" 2>"$out.stderr" || status=$?
  if [[ "$status" -ne 0 ]]; then
    printf 'barge %s exited %s\n' "$*" "$status"
    if [[ -s "$out" ]]; then
      printf 'stdout: '
      head -c 16384 "$out"
      printf '\n'
    fi
    if [[ -s "$out.stderr" ]]; then
      printf 'stderr: '
      head -c 16384 "$out.stderr"
      printf '\n'
    fi
  fi
  return "$status"
}

assert_json() {
  local file="$1" expression="$2"
  /usr/bin/python3 - "$file" "$expression" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        doc = json.load(handle)
except Exception as error:  # noqa: BLE001 - a receipt must explain any failure
    raise SystemExit(f"cannot read {sys.argv[1]}: {error}")
try:
    ok = eval(sys.argv[2], {"doc": doc, "json": json})
except Exception as error:  # noqa: BLE001
    raise SystemExit(f"assertion {sys.argv[2]} raised {error!r} for {doc}")
if not ok:
    raise SystemExit(f"assertion failed: {sys.argv[2]}\n{doc}")
PY
}

json_field() {
  # One dotted field of a JSON document, as text.
  local file="$1" field="$2"
  /usr/bin/python3 - "$file" "$field" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle:
    value = json.load(handle)
for part in sys.argv[2].split("."):
    value = value[int(part)] if part.isdigit() else value[part]
print(value)
PY
}

# ---- the harness preflight: the required descriptor probes, before every case
#
# The supervisor's own closure says nothing about whether this account can read
# the descriptor evidence custody depends on. So every case that runs is preceded
# by two required observations made through the same macOS primitive Barge's
# observer uses: `proc_pidinfo(PROC_PIDLISTFDS)` for the descriptor listing, and
# `proc_pidinfo(PROC_PIDTBSDINFO)` for the "kinfo" fact that a process's
# effective uid is this account's own.
#
#  - the sentinel probe starts one sentinel child in this run's session - the
#    shared supervisor's guarded session - lists its descriptors while it is
#    alive, then terminates and reaps it inside one fixed bound;
#  - the full census lists every enumerated same-effective-uid process, using
#    the product kinfo classification and scan bounds.
#
# Either descriptor-listing capability being unavailable is an infrastructure
# fault: the run is unqualified and aborts before any case body. The probes
# start no second supervisor, create no second session, and write nothing
# outside the run root. `crates/barge/tests/native_proof_harness.rs` extracts
# this block between the two markers and drives the probes against fixture
# inputs, so both the capability and the abort are proved below the elevated
# tier.
# >>> s6-preflight
session_id() {
  /bin/ps -o sess= -p $$ | tr -d ' '
}

descriptor_observation() {
  # usage: descriptor_observation <kind> <pid> [<identity-path>]
  #
  # One same-uid process descriptor listing, through the kernel's own primitive.
  # On success prints one line to stdout:
  #   <kind> <uid> <descriptors> <vnodes> <identity-fd> <identity-vnode>
  # and returns 0; when the kinfo fact, the listing or the identity descriptor is
  # unavailable it says why on stderr and returns 3.
  local kind="$1" pid="$2" identity="${3:-}" uid
  uid="$(/usr/bin/id -u)" || return 3
  /usr/bin/python3 - "$kind" "$pid" "$identity" "$uid" <<'PY'
import ctypes, struct, sys

PROC_PIDLISTFDS = 1
PROC_PIDTBSDINFO = 3
PROC_FDINFO_SIZE = 8
PROC_BSDINFO_SIZE = 136
PROX_FDTYPE_VNODE = 1
DESCRIPTOR_CAP = 4096

kind, pid_text, identity, uid_text = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
try:
    pid, uid = int(pid_text), int(uid_text)
except ValueError:
    print(f"the process id {pid_text!r} is not a number", file=sys.stderr)
    raise SystemExit(3)

libc = ctypes.CDLL(None, use_errno=True)
libc.proc_pidinfo.restype = ctypes.c_int
libc.proc_pidinfo.argtypes = [
    ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int
]

def kinfo(pid):
    # struct proc_bsdinfo: pbi_pid at 12 and pbi_uid (the effective uid) at 20.
    # Both are read, so a wrong offset cannot pass as a wrong uid.
    buf = ctypes.create_string_buffer(PROC_BSDINFO_SIZE)
    ctypes.set_errno(0)
    written = libc.proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, buf, PROC_BSDINFO_SIZE)
    error = ctypes.get_errno()
    if written != PROC_BSDINFO_SIZE:
        return None, f"the kinfo fact of pid {pid} is unreadable (wrote {written}, errno {error})"
    reported_pid = struct.unpack_from("<I", buf.raw, 12)[0]
    reported_uid = struct.unpack_from("<I", buf.raw, 20)[0]
    if reported_pid != pid:
        return None, f"the kinfo fact of pid {pid} names pid {reported_pid}"
    return reported_uid, None

reported_uid, problem = kinfo(pid)
if problem:
    print(problem, file=sys.stderr)
    raise SystemExit(3)
if reported_uid != uid:
    print(f"pid {pid} has effective uid {reported_uid}, not this account's {uid}", file=sys.stderr)
    raise SystemExit(3)

buf = ctypes.create_string_buffer(DESCRIPTOR_CAP * PROC_FDINFO_SIZE)
ctypes.set_errno(0)
written = libc.proc_pidinfo(
    pid, PROC_PIDLISTFDS, 0, buf, DESCRIPTOR_CAP * PROC_FDINFO_SIZE
)
error = ctypes.get_errno()
if written <= 0:
    print(f"the descriptor listing of pid {pid} is unavailable (wrote {written}, errno {error})",
          file=sys.stderr)
    raise SystemExit(3)
if written >= DESCRIPTOR_CAP * PROC_FDINFO_SIZE:
    print(f"the descriptor listing of pid {pid} reached its {DESCRIPTOR_CAP}-descriptor bound",
          file=sys.stderr)
    raise SystemExit(3)
rows = []
for index in range(written // PROC_FDINFO_SIZE):
    fd, fdtype = struct.unpack_from("<iI", buf.raw, index * PROC_FDINFO_SIZE)
    rows.append((fd, fdtype))
vnodes = sum(1 for _, fdtype in rows if fdtype == PROX_FDTYPE_VNODE)
identity_fd, identity_vnode = 0, 0
if identity:
    for fd, fdtype in rows:
        if fd == 9:
            identity_fd = 9
            identity_vnode = 1 if fdtype == PROX_FDTYPE_VNODE else 0
            break
print(f"{kind} {reported_uid} {len(rows)} {vnodes} {identity_fd} {identity_vnode}")
PY
}

preflight_supervisor() {
  # Supplemental: the pinned supervisor's bytes are still the pinned bytes, this
  # run is still in the owned scratch the guard gave it, and it is still in the
  # session captured at startup. Not a substitute for the two descriptor probes.
  local supervisor="$1" pinned="$2" expected_session="$3" actual session
  if [[ ! -f "$supervisor" || ! -r "$supervisor" ]]; then
    printf 'preflight: the supervisor %s is not a readable regular file\n' "$supervisor" >&2
    return 1
  fi
  actual="$(shasum -a 256 "$supervisor" 2>/dev/null | awk '{print $1}')"
  if [[ "$actual" != "$pinned" ]]; then
    printf 'preflight: %s is %s, not the pinned %s\n' \
      "$supervisor" "${actual:-unreadable}" "$pinned" >&2
    return 1
  fi
  if [[ -z "${OXIDE_TASK_SCRATCH_ROOT:-}" || -z "${TMPDIR:-}" ]]; then
    printf 'preflight: the guard scratch pointers are unset\n' >&2
    return 1
  fi
  case "$TMPDIR" in
    "$OXIDE_TASK_SCRATCH_ROOT"*) ;;
    *)
      printf 'preflight: TMPDIR %s left the guard scratch %s\n' \
        "$TMPDIR" "$OXIDE_TASK_SCRATCH_ROOT" >&2
      return 1
      ;;
  esac
  session="$(session_id)"
  if [[ -z "$session" || -z "$expected_session" || "$session" != "$expected_session" ]]; then
    printf 'preflight: session %s is not the guarded session %s\n' \
      "${session:-unknown}" "${expected_session:-unknown}" >&2
    return 1
  fi
  return 0
}

preflight_sentinel() {
  # Required probe one: one sentinel child in this run's session, alive while its
  # descriptors are listed, then terminated and reaped inside one fixed bound.
  # The child opens the identity file on fd 9 before exec, so the listing must
  # report that live vnode descriptor of that live same-uid process.
  local probe_dir="$1" bound="${2:-$PREFLIGHT_SENTINEL_BOUND_SECONDS}"
  local steps=$((bound * 10)) waited=0 pid identity expected observation status
  local problem="" inside_bound=1 recorded_pid recorded_session
  if ! mkdir -p "$probe_dir"; then
    printf 'preflight: cannot create the sentinel probe directory %s\n' "$probe_dir" >&2
    return 1
  fi
  identity="$probe_dir/sentinel.identity"
  rm -f "$identity"
  expected="$(session_id)"
  if [[ -z "$expected" ]]; then
    printf 'preflight: cannot read the session id of this run\n' >&2
    return 1
  fi
  # The sentinel is a plain child of this shell, in the shared supervisor's
  # guarded session, so the supervisor's census sees it and nothing else does.
  # It is never detached into a session of its own.
  /bin/sh -c 'exec 9>"$1"; printf "pid=%s session=%s\n" "$$" "$(/bin/ps -o sess= -p $$ | tr -d " ")" >&9; exec /bin/sleep "$2"' \
    _ "$identity" 30 &
  pid=$!
  while [[ ! -s "$identity" ]] && (( waited < steps )); do
    /bin/sleep 0.1
    waited=$((waited + 1))
  done
  if [[ ! -s "$identity" ]]; then
    problem="the sentinel did not record its identity inside its ${bound} s bound"
  elif ! kill -0 "$pid" 2>/dev/null; then
    problem="the sentinel exited before its ${bound} s bound"
  else
    # The required observation: this live same-uid sentinel's descriptor listing.
    observation="$(descriptor_observation sentinel "$pid" "$identity")" ||
      problem="the sentinel's descriptor listing is unavailable: $observation"
  fi
  # Terminate and reap inside the same fixed bound. The shell prints its own
  # "Terminated" job notice for a signalled child on stderr; the probe answers
  # with an explicit return instead, so that notice is silenced here rather than
  # leaking into a case log.
  {
    if kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      while kill -0 "$pid" 2>/dev/null && (( waited < steps )); do
        /bin/sleep 0.1
        waited=$((waited + 1))
      done
      if kill -0 "$pid" 2>/dev/null; then
        inside_bound=0
        kill -KILL "$pid" 2>/dev/null || true
      fi
    fi
    # Always reap: the probe's own child never outlives it.
    wait "$pid" 2>/dev/null
    status=$?
  } 2>/dev/null
  if kill -0 "$pid" 2>/dev/null; then
    printf 'preflight: the sentinel %s was not reaped\n' "$pid" >&2
    return 1
  fi
  if [[ "$inside_bound" != 1 ]]; then
    printf 'preflight: the sentinel needed KILL after its %s s bound\n' "$bound" >&2
    return 1
  fi
  if [[ -n "$problem" ]]; then
    printf 'preflight: %s\n' "$problem" >&2
    return 1
  fi
  recorded_pid="$(awk '{print $1}' "$identity" 2>/dev/null)"
  recorded_session="$(awk '{print $2}' "$identity" 2>/dev/null)"
  if [[ "$recorded_pid" != "pid=$pid" || "$recorded_session" != "session=$expected" ]]; then
    printf 'preflight: the sentinel recorded %s %s, not pid %s in session %s\n' \
      "$recorded_pid" "$recorded_session" "$pid" "$expected" >&2
    return 1
  fi
  local kind_out uid_out descriptors vnodes identity_fd identity_vnode
  read -r kind_out uid_out descriptors vnodes identity_fd identity_vnode <<<"$observation"
  if [[ "$kind_out" != sentinel || "$descriptors" -lt 1 || "$identity_fd" != 9 || "$identity_vnode" != 1 ]]; then
    printf 'preflight: the sentinel descriptor observation was %s\n' "$observation" >&2
    return 1
  fi
  PREFLIGHT_LAST_SENTINEL="$observation"
  return 0
}

preflight_existing_descriptors() {
  # Required probe two: the exact Barge executable's product observer, including
  # vnode paths and #012 re-observation, with custody's unchanged scan bounds.
  local probe_dir="$1" status=0
  barge_cli "$probe_dir/census.json" diagnostic descriptor-census --root "$probe_dir" --json || status=$?
  PREFLIGHT_LAST_CENSUS="$(/usr/bin/python3 - "$probe_dir/census.json" "$probe_dir" "$status" <<'PY_CENSUS'
import json, sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        doc = json.load(handle)
    if int(sys.argv[3]) != 0 or not doc.get("ok"):
        result = {"status": "infrastructure", "problem": doc.get("error", doc)}
    elif (doc.get("schema") != "barge.diagnostic.descriptor-census/1"
          or doc.get("command") != "diagnostic.descriptor-census"):
        result = {"status": "infrastructure", "problem": "unexpected census document"}
    else:
        result = doc["data"]
        if (result.get("root") != sys.argv[2]
                or result.get("process_limit") != 4096
                or result.get("descriptor_limit") != 65536
                or result.get("timeout_ms") != 10000
                or result.get("status") not in ("passed", "infrastructure")):
            result = {"status": "infrastructure", "problem": "census root, bounds or outcome mismatch"}
except (OSError, ValueError, KeyError, TypeError) as error:
    result = {"status": "infrastructure", "problem": str(error)}
print(json.dumps(result, separators=(",", ":")))
raise SystemExit(0 if result["status"] == "passed" else 3)
PY_CENSUS
)" || status=$?
  if [[ "$status" != 0 ]]; then
    printf 'preflight: same-Barge full same-uid census unavailable: %s\n' "$PREFLIGHT_LAST_CENSUS" >&2
    return 1
  fi
  return 0
}

preflight() {
  # Both required descriptor probes before one case, plus the supplemental
  # supervisor identity check. Either descriptor capability failing is an
  # infrastructure abort: no case body runs, the receipt is unqualified, and the
  # run exits 125 rather than reporting a case result.
  local case_id="$1" probe_dir="$2"
  if ! preflight_supervisor "$SUPERVISOR" "$SUPERVISOR_SHA256" "$PREFLIGHT_SESSION"; then
    PREFLIGHT_FAILURE="supervisor probe failed before $case_id"
    return 1
  fi
  if ! preflight_sentinel "$probe_dir" "$PREFLIGHT_SENTINEL_BOUND_SECONDS"; then
    PREFLIGHT_FAILURE="sentinel descriptor probe failed before $case_id"
    return 1
  fi
  if ! preflight_existing_descriptors "$probe_dir"; then
    printf '%s\tfull-census\t%s\n' "$case_id" "$PREFLIGHT_LAST_CENSUS" >>"$PREFLIGHT_TSV"
    PREFLIGHT_FAILURE="full same-uid descriptor census failed before $case_id: $PREFLIGHT_LAST_CENSUS"
    return 1
  fi
  PREFLIGHT_RUNS=$((PREFLIGHT_RUNS + 1))
  printf '%s\tsupervisor\tsession=%s pinned=%s\n' \
    "$case_id" "$PREFLIGHT_SESSION" "$SUPERVISOR_SHA256" >>"$PREFLIGHT_TSV"
  printf '%s\tsentinel-descriptors\t%s\n' \
    "$case_id" "$PREFLIGHT_LAST_SENTINEL" >>"$PREFLIGHT_TSV"
  printf '%s\tfull-census\t%s\n' \
    "$case_id" "$PREFLIGHT_LAST_CENSUS" >>"$PREFLIGHT_TSV"
  return 0
}
# <<< s6-preflight

# ---- the premise trace ------------------------------------------------------
#
# The design's nine-premise HIGH checklist, traced through the case bodies that
# demonstrate it. Each row is a premise, the case body that asserts it, and the
# concrete assertion in that body; the receipt folds these rows against the
# recorded case results, so a premise is traced by a case that actually passed,
# not by prose. `crates/barge/tests/native_proof_harness.rs` extracts this block
# and proves it covers every premise, every case and no case that does not exist.
# >>> s6-premise-trace
write_premise_trace() {
  local premise case_id asserts
  while IFS='|' read -r premise case_id asserts; do
    [[ -n "$premise" ]] || continue
    printf '%s\t%s\t%s\n' "$premise" "$case_id" "$asserts" >>"$PREMISE_TRACE_TSV"
  done <<'TRACE'
1|plan-effect-free|describe reports the fixture account home and the fixture state directory, so state resolves outside the real account
1|setting-written|the adopted setting and its undo read and write only the fixture home's ~/.npmrc
1|hand-made-in-place|the adopted layout is the fixture home's link, not the real ~/.cargo
2|plan-effect-free|describe's state.home.account is the fixture home, not the password database's account home
2|ordinary-detach|the absent-volume refusal is read from the fixture state directory's own registry
3|absent-path|the adopted cache target is under the case's caches folder on the owned image
3|plan-effect-free|describe's state.dir is the case's BARGE_STATE_DIR inside the run root
4|absent-path|the link is published only by an explicit cache adopt --kind --volume
4|empty-directory|the empty directory becomes a link only through the explicit adopt
4|setting-unchanged|the already-pointing setting is recorded by the explicit adopt with no byte written
4|setting-written|the setting is written only by the explicit adopt with --write-setting
4|hand-made-in-place|the existing root is adopted only with the explicit --adopt-existing-root
4|plan-effect-free|--plan reports the intended effects and adopts nothing
5|ordinary-detach|an ordinary hdiutil detach makes the cache unavailable naming the drive, and a re-attach makes it work again
6|absent-path|the adopt uses only --kind, --volume and --json
6|setting-written|the setting write requires the explicit --write-setting flag
6|hand-made-in-place|the in-place form requires the explicit --adopt-existing-root flag
6|plan-effect-free|the preview uses --plan and adopts nothing
7|absent-path|the adopted root and the link target are inside the case's caches folder on the owned image
7|empty-directory|the restored directory is the fixture home's, inside the run root
7|hand-made-in-place|the adopted root and its marker are on the owned image, under the case's caches folder
7|plan-effect-free|the state directory the CLI used is inside the run root
8|absent-path|the case's home, state and out are case-scoped under the mktemp run root
8|hand-made-in-place|the case's caches folder is case-scoped under the owned image
8|plan-effect-free|the plan case's own state directory is case-scoped
9|absent-path|the per-case preflight ran before this case with both descriptor probes
9|empty-directory|the per-case preflight ran before this case with both descriptor probes
9|setting-unchanged|the per-case preflight ran before this case with both descriptor probes
9|setting-written|the per-case preflight ran before this case with both descriptor probes
9|diverged-undo|the per-case preflight ran before this case with both descriptor probes
9|non-empty-refused|the per-case preflight ran before this case with both descriptor probes
9|hand-made-in-place|the per-case preflight ran before this case with both descriptor probes
9|plan-effect-free|the per-case preflight ran before this case with both descriptor probes
9|ordinary-detach|the per-case preflight ran before this case with both descriptor probes
TRACE
}
# <<< s6-premise-trace
write_premise_trace

# ---- the cases --------------------------------------------------------------

case_absent_path() {
  mkdir -p "$HOME/.cargo"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind cargo-registry --volume "$UUID" --json || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["plan"] is False and doc["data"]["placement"]["form"]["form"] == "link" and doc["data"]["placement"]["form"]["restore"] == "absent"' || return 1
  local target
  target="$(json_field "$CASE_OUT/adopt.json" data.placement.form.target)"
  [[ -L "$HOME/.cargo/registry" ]] || { echo "no link at the tool's path"; return 1; }
  [[ "$(readlink "$HOME/.cargo/registry")" == "$target" ]] || { echo "the link text is not the recorded target"; return 1; }
  [[ "$target" == "$CASE_CACHES/"* ]] || { echo "the target $target is not under $CASE_CACHES"; return 1; }
  barge_cli "$CASE_OUT/list.json" cache list --json || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 1 and doc["data"]["placements"]["rows"][0]["link"] == "linked"' || return 1
  barge_cli "$CASE_OUT/undo.json" cache undo --kind cargo-registry --json || return 1
  assert_json "$CASE_OUT/undo.json" 'doc["ok"] and doc["data"]["removed"] is True' || return 1
  [[ ! -e "$HOME/.cargo/registry" && ! -L "$HOME/.cargo/registry" ]] || { echo "the path is not absent again"; return 1; }
  [[ -d "$target" ]] || { echo "undo deleted the owned root"; return 1; }
  echo "link $target published and removed; the owned root is kept"
}

case_empty_directory() {
  mkdir -p "$HOME/.cargo/git"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind cargo-git --volume "$UUID" --json || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "link" and doc["data"]["placement"]["form"]["restore"] == "empty_directory"' || return 1
  [[ -L "$HOME/.cargo/git" ]] || { echo "the empty directory was not replaced by a link"; return 1; }
  barge_cli "$CASE_OUT/undo.json" cache undo --kind cargo-git --json || return 1
  [[ -d "$HOME/.cargo/git" && ! -L "$HOME/.cargo/git" ]] || { echo "the directory was not restored"; return 1; }
  [[ -z "$(ls -A "$HOME/.cargo/git")" ]] || { echo "the restored directory is not empty"; return 1; }
  echo "an empty directory became a link and was restored empty"
}

case_setting_unchanged() {
  mkdir -p "$CASE_CACHES/npm"
  printf 'cache=%s\n' "$CASE_CACHES/npm" >"$HOME/.npmrc"
  local before after
  before="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind npm-cache --volume "$UUID" --json || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "setting" and doc["data"]["placement"]["form"]["changed"] is False' || return 1
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" == "$before" ]] || { echo "adopt changed a byte of an already-pointing setting"; return 1; }
  [[ ! -e "$HOME/.npmrc.barge-backup" ]] || { echo "adopt wrote a backup for an unchanged setting"; return 1; }
  barge_cli "$CASE_OUT/undo.json" cache undo --kind npm-cache --json || return 1
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" == "$before" ]] || { echo "undo changed the unchanged setting"; return 1; }
  echo "a setting that already names the owned root is recorded with no byte written"
}

case_setting_written() {
  printf 'cache=/old/cache\nregistry=https://example.invalid\n' >"$HOME/.npmrc"
  local before after
  before="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind npm-cache --volume "$UUID" --write-setting --json || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "setting" and doc["data"]["placement"]["form"]["changed"] is True and any("cache" in change for change in doc["data"]["changes"])' || return 1
  grep -q "cache=${CASE_CACHES}/npm" "$HOME/.npmrc" || { echo "the new value is not in the setting"; return 1; }
  grep -q "registry=https://example.invalid" "$HOME/.npmrc" || { echo "an unrelated line was lost"; return 1; }
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" != "$before" ]] || { echo "the setting was not written"; return 1; }
  barge_cli "$CASE_OUT/undo.json" cache undo --kind npm-cache --json || return 1
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" == "$before" ]] || { echo "undo did not restore the exact bytes"; return 1; }
  echo "the tool setting was written only by the explicit adopt, reported, and undone byte for byte"
}

case_diverged_undo() {
  printf 'cache=/old/cache\n' >"$HOME/.npmrc"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind npm-cache --volume "$UUID" --write-setting --json || return 1
  printf 'cache=/someone/else\n' >"$HOME/.npmrc"
  local before
  before="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  local status=0
  barge_cli "$CASE_OUT/undo.json" cache undo --kind npm-cache --json || status=$?
  [[ "$status" == 3 ]] || { echo "a diverged undo exited $status, not 3"; return 1; }
  assert_json "$CASE_OUT/undo.json" 'doc["error"]["kind"] == "unknown_state"' || return 1
  [[ "$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')" == "$before" ]] || { echo "the diverged file was changed"; return 1; }
  barge_cli "$CASE_OUT/list.json" cache list --json || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 1' || return 1
  echo "a file that diverged after adopt refuses undo and is left exactly as it is"
}

case_non_empty_refused() {
  mkdir -p "$HOME/.cargo/registry"
  printf 'registry payload' >"$HOME/.cargo/registry/payload"
  local before after status=0
  before="$(digest_tree "$HOME/.cargo/registry")"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind cargo-registry --volume "$UUID" --json || status=$?
  [[ "$status" == 2 ]] || { echo "a non-empty directory exited $status, not 2"; return 1; }
  assert_json "$CASE_OUT/adopt.json" \
    'doc["error"]["kind"] == "refused_boundary" and doc["error"]["detail"]["changed"] is False and isinstance(doc["error"]["detail"]["plan"], list) and len(doc["error"]["detail"]["plan"]) >= 2' || return 1
  after="$(digest_tree "$HOME/.cargo/registry")"
  [[ "$after" == "$before" ]] || { echo "the refused directory changed"; return 1; }
  [[ ! -L "$HOME/.cargo/registry" ]] || { echo "the directory became a link"; return 1; }
  barge_cli "$CASE_OUT/list.json" cache list --json || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 0' || return 1
  echo "a non-empty real directory refused with a plan and nothing was renamed or deleted"
}

case_hand_made_in_place() {
  local target="$CASE_CACHES/cargo/registry"
  mkdir -p "$target"
  printf 'registry data' >"$target/payload"
  mkdir -p "$HOME/.cargo"
  ln -s "$target" "$HOME/.cargo/registry"
  local before before_link before_link_identity after
  before="$(digest_tree_excluding "$target" .barge-root.json)" || return 1
  before_link="$(readlink "$HOME/.cargo/registry")"
  before_link_identity="$(stat -f '%d:%i' "$HOME/.cargo/registry")"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind cargo-registry --volume "$UUID" --adopt-existing-root --json || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "held_root" and doc["data"]["placement"]["form"]["path"] == doc["data"]["placement"]["recorded_path"]' || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["data"]["placement"]["form"]["publication"]["link_path"] == doc["data"]["placement"]["original_path"] and doc["data"]["placement"]["form"]["publication"]["target"] == doc["data"]["placement"]["form"]["path"]' || return 1
  after="$(digest_tree_excluding "$target" .barge-root.json)" || return 1
  [[ "$after" == "$before" ]] || { echo "the hand-made payload or layout changed"; return 1; }
  [[ "$(readlink "$HOME/.cargo/registry")" == "$before_link" ]] || { echo "the pre-existing link was swapped"; return 1; }
  [[ "$(stat -f '%d:%i' "$HOME/.cargo/registry")" == "$before_link_identity" ]] || { echo "the pre-existing link identity changed"; return 1; }
  [[ -f "$target/.barge-root.json" ]] || { echo "no marker was written"; return 1; }
  barge_cli "$CASE_OUT/undo.json" cache undo --kind cargo-registry --json || return 1
  [[ ! -e "$target/.barge-root.json" ]] || { echo "undo left the marker"; return 1; }
  after="$(digest_tree_excluding "$target" .barge-root.json)" || return 1
  [[ "$after" == "$before" ]] || { echo "undo changed the cache data"; return 1; }
  [[ "$(readlink "$HOME/.cargo/registry")" == "$before_link" ]] || { echo "undo changed the pre-existing link"; return 1; }
  [[ "$(stat -f '%d:%i' "$HOME/.cargo/registry")" == "$before_link_identity" ]] || { echo "undo changed the pre-existing link identity"; return 1; }
  echo "today's hand-made placement was adopted in place, nothing moved, and undo removed only the marker"
}

case_plan_effect_free() {
  mkdir -p "$HOME/.cargo"
  # The account home and the state directory the CLI used are the fixtures'.
  barge_cli "$CASE_OUT/describe.json" describe --json || return 1
  assert_json "$CASE_OUT/describe.json" \
    'doc["data"]["state"]["home"]["account"] == "'"$HOME"'" and doc["data"]["state"]["dir"] == "'"$BARGE_STATE_DIR"'"' || return 1
  barge_cli "$CASE_OUT/plan.json" cache adopt --kind cargo-registry --volume "$UUID" --plan --json || return 1
  assert_json "$CASE_OUT/plan.json" \
    'doc["ok"] and doc["data"]["plan"] is True and doc["data"]["placement"] is None and len(doc["data"]["changes"]) > 0' || return 1
  [[ ! -e "$CASE_CACHES" ]] || { echo "the plan created the caches folder"; return 1; }
  [[ ! -e "$HOME/.cargo/registry" ]] || { echo "the plan created the tool's path"; return 1; }
  barge_cli "$CASE_OUT/list.json" cache list --json || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 0' || return 1
  echo "--plan reported the intended effects and wrote nothing; describe reports the fixture home $HOME and state $BARGE_STATE_DIR"
}

case_ordinary_detach() {
  mkdir -p "$HOME/.cargo"
  barge_cli "$CASE_OUT/adopt.json" cache adopt --kind cargo-registry --volume "$UUID" --json || return 1
  detach_image case || return 1
  [[ "$LAST_DETACH" == ordinary ]] || return 1
  local status=0
  barge_cli "$CASE_OUT/undo.json" cache undo --kind cargo-registry --json || status=$?
  [[ "$status" == 2 ]] || { echo "an absent volume's undo exited $status, not 2"; return 1; }
  assert_json "$CASE_OUT/undo.json" \
    'doc["error"]["kind"] == "volume_absent" and doc["error"]["detail"]["attach"] == "'"$MOUNT"'"' || return 1
  grep -q "$MOUNT" "$CASE_OUT/undo.json" || { echo "the refusal does not name the drive"; return 1; }
  status=0
  barge_cli "$CASE_OUT/adopt-absent.json" cache adopt --kind cargo-git --volume "$UUID" --json || status=$?
  [[ "$status" == 2 ]] || { echo "an absent volume's adopt exited $status, not 2"; return 1; }
  assert_json "$CASE_OUT/adopt-absent.json" 'doc["error"]["kind"] == "volume_absent"' || return 1
  [[ -L "$HOME/.cargo/registry" ]] || { echo "the refusal changed the published link"; return 1; }
  attach_image || return 1
  barge_cli "$CASE_OUT/list.json" cache list --json || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 1 and doc["data"]["placements"]["rows"][0]["link"] == "linked"' || return 1
  echo "an ordinary detach made the cache unavailable naming $MOUNT; re-attach made it work again"
}

# ---- the runner -------------------------------------------------------------

record() {
  # id, status, evidence, and the premise ids the case body demonstrates.
  printf '%s\t%s\t%s\t%s\n' "$1" "$2" \
    "$(printf '%s' "$3" | tr '\n\t' '  ' | cut -c1-400)" "$4" >>"$CASES_TSV"
}

status_of() {
  awk -v id="$1" -F'\t' '$1 == id {print $2}' "$CASES_TSV" | tail -1
}

FIRST_FAULT=""
STOPPED=""

run_case() {
  local id="$1" depends="$2" body="$3" premises="$4"
  local dir="$RUN/cases/$id"
  local log="$CASE_LOGS/$id.log"
  mkdir -p "$dir/home" "$dir/state" "$dir/out"
  if [[ -n "$STOPPED" ]]; then
    record "$id" "skipped" "the counted run stopped at $STOPPED" "$premises"
    return 0
  fi
  if [[ -n "$depends" && "$(status_of "$depends")" != "pass" ]]; then
    record "$id" "skipped" "depends on $depends, which did not pass" "$premises"
    return 0
  fi
  # The harness preflight precedes every case that runs. A failed probe is an
  # infrastructure abort, not a product result: stop every remaining case and
  # let the run's verdict refuse the receipt.
  if ! preflight "$id" "$dir/preflight"; then
    record "$id" "skipped" "infrastructure abort: $PREFLIGHT_FAILURE" "$premises"
    STOPPED="$id"
    return 0
  fi
  export HOME="$dir/home"
  export BARGE_STATE_DIR="$dir/state"
  export BARGE_ACCOUNT_HOME="$dir/home"
  CASE_OUT="$dir/out"
  CASE_CACHES="$MOUNT/cases/$id/caches"
  write_registry "$dir/state" "cases/$id" || {
    record "$id" "fail" "the fixture registry could not be written" "$premises"
    FIRST_FAULT="${FIRST_FAULT:-$id}"
    return 0
  }
  if "$body" >"$log" 2>&1; then
    record "$id" "pass" "$(tr '\n' ';' <"$log")" "$premises"
  else
    record "$id" "fail" "$(tr '\n' ';' <"$log")" "$premises"
    FIRST_FAULT="${FIRST_FAULT:-$id}"
    if [[ "$MODE" == "counted" ]]; then
      STOPPED="$id"
    fi
  fi
  return 0
}

# ---- run -------------------------------------------------------------------

hdiutil create -size "${IMAGE_MIB}m" -fs APFS -volname "$VOLNAME" -type SPARSE -quiet "$BACKING" >/dev/null ||
  refuse "cannot create the owned sparse image"
attach_image || refuse "cannot attach the owned image at $MOUNT"
UUID="$(diskutil info -plist "$MOUNT" | plutil -extract VolumeUUID raw -o - -)" ||
  refuse "cannot read the image's volume UUID"
readonly UUID

# The last argument is the set of premise ids the body demonstrates, as traced in
# write_premise_trace. The receipt folds them against the recorded results.
run_case absent-path "" case_absent_path "3,4,6,7,8,9"
run_case empty-directory "" case_empty_directory "4,7,9"
run_case setting-unchanged "" case_setting_unchanged "4,9"
run_case setting-written "" case_setting_written "1,4,6,9"
run_case diverged-undo setting-written case_diverged_undo "9"
run_case non-empty-refused "" case_non_empty_refused "9"
run_case hand-made-in-place "" case_hand_made_in_place "1,4,6,7,8,9"
run_case plan-effect-free "" case_plan_effect_free "1,2,3,4,6,7,8,9"
run_case ordinary-detach absent-path case_ordinary_detach "2,5,9"

# The run's own verdict: a counted pass needs every case, a preflight before
# every case, safe custody and an untouched real account state; the diagnostic
# twin never qualifies. A failed preflight is an infrastructure abort: the harness
# is unsound, so nothing the run did counts and the receipt is refused.
if [[ -n "$PREFLIGHT_FAILURE" ]]; then
  refuse "preflight infrastructure abort: $PREFLIGHT_FAILURE; no case result counts"
fi
if [[ -n "$FIRST_FAULT" ]]; then
  printf 's6: %s: first fault %s; see %s\n' "$MODE" "$FIRST_FAULT" "$RECEIPT" >&2
  exit 1
fi
exit 0
