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
# (scripts/bounded-test.py, bytes pinned below), it creates one private run root
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
readonly SUPERVISOR_PATH_DEFAULT="${HOME}/Dev/projects/oxide/scripts/bounded-test.py"
readonly SUPERVISOR_SHA256="bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924"

# The elevated tier's fixed resources.
readonly LIMIT_SECONDS=300
readonly SCRATCH_BUDGET_MIB=512
readonly IMAGE_MIB=128

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
  if [[ ! -x "$SUPERVISOR" ]]; then
    printf 's6: the qualified supervisor %s is not executable; this proof runs only under it\n' \
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
  exec /usr/bin/python3 "$SUPERVISOR" \
    --limit "$LIMIT_SECONDS" \
    --scratch-budget-mib "$SCRATCH_BUDGET_MIB" \
    -- "$SCRIPT_PATH" "$MODE" "$BARGE_BIN_INPUT"
fi

# ---- the inner half: the guarded run ----------------------------------------

readonly EVIDENCE="$OXIDE_PROOF_EVIDENCE"
readonly RECEIPT="$EVIDENCE/s6-cache-placement-receipt.json"

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

STARTED_MS="$(/usr/bin/python3 -c 'import time;print(int(time.time()*1000))')"
readonly STARTED_MS
RUN="$(mktemp -d "$TMPDIR/barge-s6-run.XXXXXX")" || refuse "cannot create the run root"
readonly RUN
readonly CASES_TSV="$RUN/cases.tsv"
: >"$CASES_TSV"

# Premise 3: every inherited scratch/spool pointer moves inside the run root.
export HOME="$RUN/home"
export CARGO_HOME="$RUN/cargo-home"
export BARGE_STATE_DIR="$RUN/state"
export BARGE_ACCOUNT_HOME="$RUN/home"
export TMP="$TMPDIR"
export TEMP="$TMPDIR"
unset CARGO_TARGET_DIR || true
mkdir -p "$HOME" "$CARGO_HOME" "$BARGE_STATE_DIR" "$RUN/backing" "$RUN/cases"

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

digest_tree_excluding() {
  # The same digest with one basename left out, for a tree whose only new entry
  # is the marker Barge writes.
  local path="$1" excluded="$2"
  (
    cd "$path" || exit 1
    find . ! -name "$excluded" -exec stat -f 'S %N %HT %z %m' {} \; 2>/dev/null | LC_ALL=C sort
    find . -type f ! -name "$excluded" -print0 2>/dev/null |
      xargs -0 shasum -a 256 2>/dev/null | LC_ALL=C sort
  ) | shasum -a 256 | awk '{print $1}'
}

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

attach_image() {
  local output
  output="$(hdiutil attach -owners on -nobrowse -mountpoint "$MOUNT" "$BACKING" 2>&1)" || {
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

detach_image() {
  [[ "$ATTACHED" == 1 ]] || return 0
  if hdiutil detach "$MOUNT" >/dev/null 2>&1; then
    ATTACHED=0
    return 0
  fi
  # Premise 5: a forced detach is allowed only for this image's own whole disk.
  local whole
  whole="$(diskutil info "$DEVICE" 2>/dev/null | awk -F': *' '/Part of Whole/{print $2}')"
  if [[ -n "$whole" ]] && hdiutil detach "/dev/$whole" -force >/dev/null 2>&1; then
    ATTACHED=0
    return 0
  fi
  printf 'ordinary detach failed; the backing file is kept at %s\n' "$BACKING" >&2
  KEEP_BACKING=1
  return 1
}

# ---- the receipt ------------------------------------------------------------

write_receipt() {
  local exit_status="$1"
  S6_MODE="$MODE" \
  S6_EXIT="$exit_status" \
  S6_RECEIPT="$RECEIPT" \
  S6_EVIDENCE="$EVIDENCE" \
  S6_CASES="$CASES_TSV" \
  S6_HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || printf 'unknown')" \
  S6_REPO="$REPO_ROOT" \
  S6_BINARY="$BARGE_BIN" \
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
  /usr/bin/python3 - <<'PY'
import json, os, sys, time

mode = os.environ["S6_MODE"]
cases = []
path = os.environ["S6_CASES"]
if os.path.exists(path):
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3:
                cases.append({"id": parts[0], "status": parts[1], "evidence": parts[2]})

passed = sum(1 for case in cases if case["status"] == "pass")
failed = sum(1 for case in cases if case["status"] == "fail")
skipped = sum(1 for case in cases if case["status"] == "skipped")
first_fault = next((case["id"] for case in cases if case["status"] == "fail"), None)

premises = [
    {
        "id": 1,
        "premise": "A private HOME, CARGO_HOME and state directory, so no real account state is read or written.",
        "trace": ["every case"],
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
        "trace": ["every case", "receipt: real_state"],
        "evidence": {
            "account_home": os.environ["S6_HOME"],
            "environment_home": os.environ["S6_HOME"],
            "real_state": os.environ["S6_REAL_STATE"],
            "before": os.environ["S6_REAL_BEFORE"],
        },
    },
    {
        "id": 3,
        "premise": "Environment isolation: BARGE_STATE_DIR, TMPDIR/TMP/TEMP and every inherited scratch point inside the guard's owned scratch; no inherited real paths.",
        "trace": ["guarded startup assertion", "every case"],
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
        "trace": ["plan-effect-free", "absent-path", "empty-directory", "setting-unchanged", "hand-made-in-place"],
        "evidence": {"plan_case": "plan-effect-free", "explicit_flag": "--adopt-existing-root"},
    },
    {
        "id": 5,
        "premise": "Owned image with ownership enabled (-owners on), ordinary detach, never a forced detach of anything but the owned image's own whole-disk device.",
        "trace": ["startup attach", "ordinary-detach", "cleanup"],
        "evidence": {
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
            "hooks": [
                {"name": "OXIDE_TASK_SCRATCH_ROOT", "purpose": "the supervisor's owned scratch; every fixture lives inside it"},
                {"name": "TMPDIR/TMP/TEMP", "purpose": "the guard's owned scratch, inherited from the supervisor"},
                {"name": "OXIDE_PROOF_EVIDENCE", "purpose": "where this receipt is written"},
                {"name": "BARGE_STATE_DIR", "purpose": "the product's documented fixture-only state directory"},
                {"name": "BARGE_ACCOUNT_HOME", "purpose": "the product's documented fixture-only account home; a normal run leaves it unset"},
                {"name": "HOME/CARGO_HOME", "purpose": "the private fixture home and cargo home"},
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
        "trace": ["mktemp run root", "every case", "cleanup"],
        "evidence": {"scratch": os.environ["S6_SCRATCH"], "run_root": os.path.dirname(os.environ["S6_STATE"])},
    },
    {
        "id": 8,
        "premise": "mktemp -d under that root for every fixture directory, never a shared name.",
        "trace": ["run root", "per-case home/state/out"],
        "evidence": {"run_root_pattern": "barge-s6-run.XXXXXX", "case_dirs": "cases/<case-id>/{home,state,out}"},
    },
    {
        "id": 9,
        "premise": "The qualified runner bytes and selector are named and hash-pinned; Barge adds no second supervisor, session or accounting harness.",
        "trace": ["startup supervisor pin", "effigy selector cache:native / cache:native:diagnostic"],
        "evidence": {
            "supervisor": os.environ["S6_SUPERVISOR"],
            "supervisor_sha256": os.environ["S6_SUPERVISOR_SHA"],
            "limit_seconds": int(os.environ["S6_LIMIT"]),
            "scratch_budget_mib": int(os.environ["S6_SCRATCH_MIB"]),
            "second_supervisor": False,
        },
    },
]

real_after = os.environ["S6_REAL_AFTER"]
unchanged = real_after == os.environ["S6_REAL_BEFORE"]

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

qualified = (
    mode == "counted"
    and failed == 0
    and skipped == 0
    and not_run == []
    and unchanged
    and int(os.environ["S6_EXIT"]) == 0
)

receipt = {
    "schema": "barge.s6.native/1",
    "qualified": qualified,
    "mode": mode,
    "head": os.environ["S6_HEAD"],
    "repo": os.environ["S6_REPO"],
    "binary": os.environ["S6_BINARY"],
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
    "real_state": {
        "path": os.environ["S6_REAL_STATE"],
        "before": os.environ["S6_REAL_BEFORE"],
        "after": real_after,
        "unchanged": unchanged,
    },
    "premises": premises,
    "cases": cases,
    "summary": summary,
    "started_ms": int(os.environ["S6_STARTED"]),
    "finished_ms": int(time.time() * 1000),
    "exit_status": int(os.environ["S6_EXIT"]),
}

with open(os.environ["S6_RECEIPT"], "w", encoding="utf-8") as handle:
    json.dump(receipt, handle, indent=1, sort_keys=False)
    handle.write("\n")

print(json.dumps({"schema": receipt["schema"], "qualified": qualified, "mode": mode,
                  "head": receipt["head"], "summary": summary,
                  "receipt": os.environ["S6_RECEIPT"]}, separators=(",", ":")))
PY
}

cleanup() {
  local status=$?
  detach_image || true
  REAL_STATE_AFTER="$(digest_tree "$REAL_STATE")"
  write_receipt "$status" || printf 's6: the receipt could not be written\n' >&2
  if [[ "$KEEP_BACKING" == 0 && "$ATTACHED" == 0 ]]; then
    rm -rf "$RUN"
  else
    printf 's6: kept the run root %s (image %s)\n' "$RUN" "$BACKING" >&2
  fi
  exit "$status"
}
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

# ---- the cases --------------------------------------------------------------

case_absent_path() {
  mkdir -p "$HOME/.cargo"
  "$BARGE_BIN" cache adopt --kind cargo-registry --volume "$UUID" --json >"$CASE_OUT/adopt.json" || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["plan"] is False and doc["data"]["placement"]["form"]["form"] == "link" and doc["data"]["placement"]["form"]["restore"] == "absent"' || return 1
  local target
  target="$(json_field "$CASE_OUT/adopt.json" data.placement.form.target)"
  [[ -L "$HOME/.cargo/registry" ]] || { echo "no link at the tool's path"; return 1; }
  [[ "$(readlink "$HOME/.cargo/registry")" == "$target" ]] || { echo "the link text is not the recorded target"; return 1; }
  [[ "$target" == "$CASE_CACHES/"* ]] || { echo "the target $target is not under $CASE_CACHES"; return 1; }
  "$BARGE_BIN" cache list --json >"$CASE_OUT/list.json" || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 1 and doc["data"]["placements"]["rows"][0]["link"] == "linked"' || return 1
  "$BARGE_BIN" cache undo --kind cargo-registry --json >"$CASE_OUT/undo.json" || return 1
  assert_json "$CASE_OUT/undo.json" 'doc["ok"] and doc["data"]["removed"] is True' || return 1
  [[ ! -e "$HOME/.cargo/registry" && ! -L "$HOME/.cargo/registry" ]] || { echo "the path is not absent again"; return 1; }
  [[ -d "$target" ]] || { echo "undo deleted the owned root"; return 1; }
  echo "link $target published and removed; the owned root is kept"
}

case_empty_directory() {
  mkdir -p "$HOME/.cargo/git"
  "$BARGE_BIN" cache adopt --kind cargo-git --volume "$UUID" --json >"$CASE_OUT/adopt.json" || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "link" and doc["data"]["placement"]["form"]["restore"] == "empty_directory"' || return 1
  [[ -L "$HOME/.cargo/git" ]] || { echo "the empty directory was not replaced by a link"; return 1; }
  "$BARGE_BIN" cache undo --kind cargo-git --json >"$CASE_OUT/undo.json" || return 1
  [[ -d "$HOME/.cargo/git" && ! -L "$HOME/.cargo/git" ]] || { echo "the directory was not restored"; return 1; }
  [[ -z "$(ls -A "$HOME/.cargo/git")" ]] || { echo "the restored directory is not empty"; return 1; }
  echo "an empty directory became a link and was restored empty"
}

case_setting_unchanged() {
  mkdir -p "$CASE_CACHES/npm"
  printf 'cache=%s\n' "$CASE_CACHES/npm" >"$HOME/.npmrc"
  local before after
  before="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  "$BARGE_BIN" cache adopt --kind npm-cache --volume "$UUID" --json >"$CASE_OUT/adopt.json" || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "setting" and doc["data"]["placement"]["form"]["changed"] is False' || return 1
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" == "$before" ]] || { echo "adopt changed a byte of an already-pointing setting"; return 1; }
  [[ ! -e "$HOME/.npmrc.barge-backup" ]] || { echo "adopt wrote a backup for an unchanged setting"; return 1; }
  "$BARGE_BIN" cache undo --kind npm-cache --json >"$CASE_OUT/undo.json" || return 1
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" == "$before" ]] || { echo "undo changed the unchanged setting"; return 1; }
  echo "a setting that already names the owned root is recorded with no byte written"
}

case_setting_written() {
  printf 'cache=/old/cache\nregistry=https://example.invalid\n' >"$HOME/.npmrc"
  local before after
  before="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  "$BARGE_BIN" cache adopt --kind npm-cache --volume "$UUID" --write-setting --json >"$CASE_OUT/adopt.json" || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "setting" and doc["data"]["placement"]["form"]["changed"] is True and any("cache" in change for change in doc["data"]["changes"])' || return 1
  grep -q "cache=${CASE_CACHES}/npm" "$HOME/.npmrc" || { echo "the new value is not in the setting"; return 1; }
  grep -q "registry=https://example.invalid" "$HOME/.npmrc" || { echo "an unrelated line was lost"; return 1; }
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" != "$before" ]] || { echo "the setting was not written"; return 1; }
  "$BARGE_BIN" cache undo --kind npm-cache --json >"$CASE_OUT/undo.json" || return 1
  after="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  [[ "$after" == "$before" ]] || { echo "undo did not restore the exact bytes"; return 1; }
  echo "the tool setting was written only by the explicit adopt, reported, and undone byte for byte"
}

case_diverged_undo() {
  printf 'cache=/old/cache\n' >"$HOME/.npmrc"
  "$BARGE_BIN" cache adopt --kind npm-cache --volume "$UUID" --write-setting --json >"$CASE_OUT/adopt.json" || return 1
  printf 'cache=/someone/else\n' >"$HOME/.npmrc"
  local before
  before="$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')"
  local status=0
  "$BARGE_BIN" cache undo --kind npm-cache --json >"$CASE_OUT/undo.json" 2>/dev/null || status=$?
  [[ "$status" == 3 ]] || { echo "a diverged undo exited $status, not 3"; return 1; }
  assert_json "$CASE_OUT/undo.json" 'doc["error"]["kind"] == "unknown_state"' || return 1
  [[ "$(shasum -a 256 "$HOME/.npmrc" | awk '{print $1}')" == "$before" ]] || { echo "the diverged file was changed"; return 1; }
  "$BARGE_BIN" cache list --json >"$CASE_OUT/list.json" || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 1' || return 1
  echo "a file that diverged after adopt refuses undo and is left exactly as it is"
}

case_non_empty_refused() {
  mkdir -p "$HOME/.cargo/registry"
  printf 'registry payload' >"$HOME/.cargo/registry/payload"
  local before after status=0
  before="$(digest_tree "$HOME/.cargo/registry")"
  "$BARGE_BIN" cache adopt --kind cargo-registry --volume "$UUID" --json >"$CASE_OUT/adopt.json" 2>/dev/null || status=$?
  [[ "$status" == 2 ]] || { echo "a non-empty directory exited $status, not 2"; return 1; }
  assert_json "$CASE_OUT/adopt.json" \
    'doc["error"]["kind"] == "refused_boundary" and doc["error"]["detail"]["changed"] is False and isinstance(doc["error"]["detail"]["plan"], list) and len(doc["error"]["detail"]["plan"]) >= 2' || return 1
  after="$(digest_tree "$HOME/.cargo/registry")"
  [[ "$after" == "$before" ]] || { echo "the refused directory changed"; return 1; }
  [[ ! -L "$HOME/.cargo/registry" ]] || { echo "the directory became a link"; return 1; }
  "$BARGE_BIN" cache list --json >"$CASE_OUT/list.json" || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 0' || return 1
  echo "a non-empty real directory refused with a plan and nothing was renamed or deleted"
}

case_hand_made_in_place() {
  local target="$CASE_CACHES/cargo/registry"
  mkdir -p "$target"
  printf 'registry data' >"$target/payload"
  ln -s "$target" "$HOME/.cargo/registry"
  local before before_link after
  before="$(digest_tree "$target")"
  before_link="$(readlink "$HOME/.cargo/registry")"
  "$BARGE_BIN" cache adopt --kind cargo-registry --volume "$UUID" --adopt-existing-root --json >"$CASE_OUT/adopt.json" || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["ok"] and doc["data"]["placement"]["form"]["form"] == "held_root" and doc["data"]["placement"]["form"]["path"] == doc["data"]["placement"]["recorded_path"]' || return 1
  assert_json "$CASE_OUT/adopt.json" \
    'doc["data"]["placement"]["form"]["publication"]["link_path"] == doc["data"]["placement"]["original_path"] and doc["data"]["placement"]["form"]["publication"]["target"] == doc["data"]["placement"]["form"]["path"]' || return 1
  after="$(digest_tree_excluding "$target" .barge-root.json)"
  [[ "$after" == "$before" ]] || { echo "the hand-made layout moved"; return 1; }
  [[ "$(readlink "$HOME/.cargo/registry")" == "$before_link" ]] || { echo "the pre-existing link was swapped"; return 1; }
  [[ -f "$target/.barge-root.json" ]] || { echo "no marker was written"; return 1; }
  "$BARGE_BIN" cache undo --kind cargo-registry --json >"$CASE_OUT/undo.json" || return 1
  [[ ! -e "$target/.barge-root.json" ]] || { echo "undo left the marker"; return 1; }
  [[ "$(digest_tree "$target")" == "$before" ]] || { echo "undo changed the cache data"; return 1; }
  [[ "$(readlink "$HOME/.cargo/registry")" == "$before_link" ]] || { echo "undo changed the pre-existing link"; return 1; }
  echo "today's hand-made placement was adopted in place, nothing moved, and undo removed only the marker"
}

case_plan_effect_free() {
  mkdir -p "$HOME/.cargo"
  # The account home and the state directory the CLI used are the fixtures'.
  "$BARGE_BIN" describe --json >"$CASE_OUT/describe.json" || return 1
  assert_json "$CASE_OUT/describe.json" \
    'doc["data"]["state"]["home"]["account"] == "'"$HOME"'" and doc["data"]["state"]["dir"] == "'"$BARGE_STATE_DIR"'"' || return 1
  "$BARGE_BIN" cache adopt --kind cargo-registry --volume "$UUID" --plan --json >"$CASE_OUT/plan.json" || return 1
  assert_json "$CASE_OUT/plan.json" \
    'doc["ok"] and doc["data"]["plan"] is True and doc["data"]["placement"] is None and len(doc["data"]["changes"]) > 0' || return 1
  [[ ! -e "$CASE_CACHES" ]] || { echo "the plan created the caches folder"; return 1; }
  [[ ! -e "$HOME/.cargo/registry" ]] || { echo "the plan created the tool's path"; return 1; }
  "$BARGE_BIN" cache list --json >"$CASE_OUT/list.json" || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 0' || return 1
  echo "--plan reported the intended effects and wrote nothing; describe reports the fixture home $HOME and state $BARGE_STATE_DIR"
}

case_ordinary_detach() {
  mkdir -p "$HOME/.cargo"
  "$BARGE_BIN" cache adopt --kind cargo-registry --volume "$UUID" --json >"$CASE_OUT/adopt.json" || return 1
  detach_image || return 1
  local status=0
  "$BARGE_BIN" cache undo --kind cargo-registry --json >"$CASE_OUT/undo.json" 2>/dev/null || status=$?
  [[ "$status" == 2 ]] || { echo "an absent volume's undo exited $status, not 2"; return 1; }
  assert_json "$CASE_OUT/undo.json" \
    'doc["error"]["kind"] == "volume_absent" and doc["error"]["detail"]["attach"] == "'"$MOUNT"'"' || return 1
  grep -q "$MOUNT" "$CASE_OUT/undo.json" || { echo "the refusal does not name the drive"; return 1; }
  status=0
  "$BARGE_BIN" cache adopt --kind cargo-git --volume "$UUID" --json >"$CASE_OUT/adopt-absent.json" 2>/dev/null || status=$?
  [[ "$status" == 2 ]] || { echo "an absent volume's adopt exited $status, not 2"; return 1; }
  assert_json "$CASE_OUT/adopt-absent.json" 'doc["error"]["kind"] == "volume_absent"' || return 1
  [[ -L "$HOME/.cargo/registry" ]] || { echo "the refusal changed the published link"; return 1; }
  attach_image || return 1
  "$BARGE_BIN" cache list --json >"$CASE_OUT/list.json" || return 1
  assert_json "$CASE_OUT/list.json" 'doc["data"]["placements"]["count"] == 1 and doc["data"]["placements"]["rows"][0]["link"] == "linked"' || return 1
  echo "an ordinary detach made the cache unavailable naming $MOUNT; re-attach made it work again"
}

# ---- the runner -------------------------------------------------------------

record() {
  printf '%s\t%s\t%s\n' "$1" "$2" "$(printf '%s' "$3" | tr '\n\t' '  ' | cut -c1-400)" >>"$CASES_TSV"
}

status_of() {
  awk -v id="$1" -F'\t' '$1 == id {print $2}' "$CASES_TSV" | tail -1
}

FIRST_FAULT=""
STOPPED=""

run_case() {
  local id="$1" depends="$2" body="$3"
  local dir="$RUN/cases/$id"
  local log="$dir/out/case.log"
  mkdir -p "$dir/home" "$dir/state" "$dir/out"
  if [[ -n "$STOPPED" ]]; then
    record "$id" "skipped" "the counted run stopped at $STOPPED"
    return 0
  fi
  if [[ -n "$depends" && "$(status_of "$depends")" != "pass" ]]; then
    record "$id" "skipped" "depends on $depends, which did not pass"
    return 0
  fi
  export HOME="$dir/home"
  export BARGE_STATE_DIR="$dir/state"
  export BARGE_ACCOUNT_HOME="$dir/home"
  CASE_OUT="$dir/out"
  CASE_CACHES="$MOUNT/cases/$id/caches"
  write_registry "$dir/state" "cases/$id" || {
    record "$id" "fail" "the fixture registry could not be written"
    FIRST_FAULT="${FIRST_FAULT:-$id}"
    return 0
  }
  if "$body" >"$log" 2>&1; then
    record "$id" "pass" "$(tr '\n' ';' <"$log")"
  else
    record "$id" "fail" "$(tr '\n' ';' <"$log")"
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

run_case absent-path "" case_absent_path
run_case empty-directory "" case_empty_directory
run_case setting-unchanged "" case_setting_unchanged
run_case setting-written "" case_setting_written
run_case diverged-undo setting-written case_diverged_undo
run_case non-empty-refused "" case_non_empty_refused
run_case hand-made-in-place "" case_hand_made_in_place
run_case plan-effect-free "" case_plan_effect_free
run_case ordinary-detach absent-path case_ordinary_detach

# The run's own verdict: a counted pass needs every case, safe custody and an
# untouched real account state; the diagnostic twin never qualifies.
if [[ -n "$FIRST_FAULT" ]]; then
  printf 's6: %s: first fault %s; see %s\n' "$MODE" "$FIRST_FAULT" "$RECEIPT" >&2
  exit 1
fi
exit 0
