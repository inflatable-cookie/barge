# Host-job hand-off: `barge maintain`

This contract owns how a host job calls Barge's one command. It is the S7 slice's
hand-off from the design's [scheduling
section](stage1-design.md#scheduling-without-a-daemon), and it is the part of
[O-006](stage1-design.md#open-design-issues) that can be written down before any
host is changed.

**State: not installed.** Barge owns no timer, daemon, watcher or LaunchAgent.
The example plist in [`examples/com.example.barge.maintain.plist`](examples/com.example.barge.maintain.plist)
is documentation: writing `~/Library/LaunchAgents/<label>.plist`, loading it,
choosing its schedule and retiring `com.tom.scratch-reaper` are operator host
changes with their own gate. Nothing in this repository installs, loads or
enables it.

## The command

```text
/usr/local/bin/barge maintain --json [--plan-out <file>]
/usr/local/bin/barge maintain --json --plan <file> --plan-sha256 <hex>
```

The first form is the preview and is effect-free: it prints one bounded canonical
`barge.maintain.plan/1`, its SHA-256 in `data.plan_digest`, and the number of
units it would delete in `data.candidates`. `--plan-out` also writes exactly the
bytes the digest covers to the named file, for a caller that wants to keep them.

The second form is the real run, and it needs both of the preview's own facts:
its exact bytes (`--plan <file>`) and their digest (`--plan-sha256 <hex>`). Either
one alone is a usage refusal before any effect. The plan names exactly the units
the run may act on, and every one of them is re-verified immediately before its
effect, so a unit that changed between the preview and the real run is kept and
reported as that unit only while the rest of the plan still runs.

## What the job does

1. Run the preview and record its exit code. A preview that prints a
   `barge.maintain/1` document with `ok: true` **completed**: `data.candidates`
   and `data.plan_digest` are its report. A preview that prints a
   `barge.error/1` document refused or could not report, and nothing was planned.
2. **Only** when the preview printed a plan document and `data.candidates` is
   greater than zero, run the real step once, passing that preview's exact bytes
   (`--plan`) and its `data.plan_digest` (`--plan-sha256`). Candidates are the
   only trigger: a preview that names none has nothing for a real run to settle
   from this plan, and the job runs no real step.
3. Record the real step's exit code. Never retry it, never loop, never run it
   twice, and never run it with a digest from an earlier day's preview.
4. Exit with the worse of the two recorded codes, so a loud absence is never
   lost and a later success never hides an earlier refusal.
5. Do nothing else. The job sweeps scratch and applies each declared area's prune
   policy; it never runs Oxide's outputs prune, never reads an Oxide receipt or
   journal, and never deletes an Oxide root.

The distinction in step 1 is what lets one volume's absence be loud without
stopping the other volumes' work: an absent **permanent** volume makes the
preview exit `3` while it still reports an authorized plan for the volumes that
are attached, so the job performs that authorized real step and records the `3`.
A pending intent is reported by the preview — never silently: the document names
it in `data.pending` and forces exit `3` even when the plan names no candidate —
and it is finished by the next real run's own replay, not by a retry. A
pending-only preview therefore records its `3` and runs no real step: the next
night the same report appears until a run with a candidate replays the intent, or
the operator settles it deliberately.

## Future space enablement

After real-drive qualification, T9 may separately authorize an operator to add
`--space` to both the preview and real invocations. At that future enablement,
the job runs the real step exactly once when `data.candidates > 0` **or**
`data.space_triggered > 0`, passing the same flags, exact plan bytes and digest.
It never retries. A volume that no longer triggers is remeasured in the real run.
Until T9 the current commands and candidate-only trigger above stay in force;
Oxide's prune entry stays until stage 4.

## Exit codes and what the job records

| Exit | Document | Meaning for the job |
| --- | --- | --- |
| `0` | plan | Completed, nothing pending. |
| `2` | error | Refused before any effect: a usage error, a plan that is not the plan or whose embedded scratch step is not self-consistent, a lock, a custody refusal. Record and stop for this schedule. |
| `3` | plan | Completed and it needs a human: an absent **permanent** volume, a volume whose durability is undeclared, a pending intent or an unsettled replay on a volume it did not touch. The plan's candidates are still authorized, so do the real step and record the `3`. |
| `3` | error | Unsettled or unknown state: the wall clock, a run that stopped on an error after its first effect (reported in `data.stopped`, never as a "refused before any effect" `2`), or a refusal that could not be classified as `2`. Record and stop. |

A non-zero exit is recorded and never retried by Barge. The recording is the
host's: launchd's `StandardOutPath`/`StandardErrorPath` append one line per run,
and Barge writes nothing about the job into its own state.

An absent `ephemeral` volume is not a failure: it is reported in the plan with
`loud: false`, and the run exits `0`. An absent `permanent` volume is reported
with `loud: true` and forces exit `3`, because a drive that should be attached and
is not needs a human. In both cases the other volumes are still maintained, the
run's own `candidates` are still authorized, and no mount point is ever
recreated.

## What the job must not do

- Install, reload or edit itself, or change the schedule, from inside the run.
- Run `maintain` concurrently with itself, or with anything that holds Barge's
  volume locks for a long time. Barge's own locks keep it safe, but the job is
  scheduled to avoid the contention rather than to discover it.
- Pass an operator list, a unit name, a path or a `--force`. There is no force
  path: an unknown is repaired by an explicit operator command, never by a retry
  with wider parameters.
- Read, move or delete anything under Oxide's `oxide-outputs/` folder or its
  receipts. Oxide's outputs prune entry stays in the existing host job until
  stage 4, and `maintain` never touches it.
- Write into a home folder, a drive root or a fixed `/tmp/<name>` path. A caller
  that wants the plan's bytes names a path inside its own temporary directory.

## The example plist

[`examples/com.example.barge.maintain.plist`](examples/com.example.barge.maintain.plist)
is the shape of such a job: `RunAtLoad` false, one daily `StartCalendarInterval`,
the binary path as a variable, the preview-then-real protocol inline, and launchd's
own log paths. It is an example, not an installation: its label, its schedule, its
binary path and its log paths all change when an operator installs one, and the
label deliberately carries `example` so it can never be mistaken for an installed
job.

The handover proof the design asks for before the host job is changed is one
`barge maintain --plan` run compared against the current reaper's dry run, so a
missing grant shows up as a reported leftover rather than a silent leak. That
comparison needs a real drive and is an operator step, not a code slice.

## Builds space enablement change plan

This is a proposed change only. It requires completed real-drive qualification,
Tom's explicit go for the host change, and a separately approved installation of
the reviewed Barge binary. The live `com.tom.scratch-reaper` plist observed for
this plan has one `ProgramArguments` element naming
`/Users/tom/.local/share/scratch-reaper/scratch-reaper`; it runs at 04:30 and
logs under that same directory. Re-read and hash the live plist and reaper before
applying; if either has changed, stop and re-review the diff. The precise plist
change, leaving every other key unchanged, is:

```diff
-  <array><string>/Users/tom/.local/share/scratch-reaper/scratch-reaper</string></array>
+  <array><string>/Users/tom/.local/share/scratch-reaper/barge-space-job</string></array>
```

The proposed new executable `barge-space-job` is the following complete script.
It runs the existing reaper first, preserving its own Oxide outputs prune, then
one Barge preview and at most one Barge run. It uses a fresh plan directory on
Builds and retains evidence for operator review; no automatic cleanup occurs.
The Barge binary path is the separately installed, reviewed binary, whose hash
must be checked against its install receipt before enablement. The job must not
be installed until that path exists and is approved.

```sh
#!/bin/sh
set -u
REAPER=/Users/tom/.local/share/scratch-reaper/scratch-reaper
BARGE=/Users/tom/.local/bin/barge
PARENT=/Volumes/Builds/scratch/barge
"$REAPER"
reaper_exit=$?
if [ "$reaper_exit" -ne 0 ]; then
  echo "reaper exit=$reaper_exit; Barge skipped"
  exit "$reaper_exit"
fi
WORK=$(/usr/bin/mktemp -d "$PARENT/host-maintain.XXXXXXXX") || exit 2
"$BARGE" maintain --space --json --plan-out "$WORK/plan.json" >"$WORK/preview.json" 2>"$WORK/preview.err"
preview_exit=$?
# A completed exit-3 plan is actionable but remains a human alert. An error
# document, a malformed plan, or an exit other than 0 or 3 stops this schedule.
case "$preview_exit" in 0|3) ;; *) echo "preview exit=$preview_exit packet=$WORK"; exit "$preview_exit";; esac
VALUES=$(/usr/bin/python3 - "$WORK/preview.json" "$WORK/plan.json" <<'PY'
import hashlib, json, pathlib, sys
try:
    doc = json.loads(pathlib.Path(sys.argv[1]).read_bytes())
    data = doc['data']; digest = data['plan_digest']
    candidates = data['candidates']; triggered = data['space_triggered']
    assert doc['schema'] == 'barge.maintain/1' and doc['ok'] is True
    assert type(candidates) is int and candidates >= 0
    assert type(triggered) is int and triggered >= 0
    assert hashlib.sha256(pathlib.Path(sys.argv[2]).read_bytes()).hexdigest() == digest
    print(digest, candidates, triggered)
except Exception:
    sys.exit(1)
PY
) || { echo "invalid preview exit=$preview_exit packet=$WORK"; exit 3; }
set -- $VALUES
DIGEST=$1 CANDIDATES=$2 TRIGGERED=$3
if [ "$CANDIDATES" -eq 0 ] && [ "$TRIGGERED" -eq 0 ]; then
  echo "preview exit=$preview_exit candidates=0 space_triggered=0 packet=$WORK"
  exit "$preview_exit"
fi
"$BARGE" maintain --space --json --plan "$WORK/plan.json" --plan-sha256 "$DIGEST" >"$WORK/real.json" 2>"$WORK/real.err"
real_exit=$?
result=$real_exit
if [ "$preview_exit" -gt "$result" ]; then result=$preview_exit; fi
echo "preview exit=$preview_exit real exit=$real_exit candidates=$CANDIDATES space_triggered=$TRIGGERED digest=$DIGEST packet=$WORK"
exit "$result"
```

Before enabling, compare a Barge `maintain --space --json` preview against the
current reaper's `/usr/bin/python3 /Users/tom/.local/share/scratch-reaper/reap.py --dry-run`.
The reaper's dry run appends its own log and can create a baseline if absent;
this is an operator host action, not the effect-free T9a preview. Capture both
outputs, exit codes and hashes, and account for every reaper candidate missing
from Barge as a reported leftover; no new grant is implied. Stop on unreadable,
incomplete, unexpected candidates or exit 3 pending operator investigation.
The job's completed exit-3 plan semantics remain those above: it runs the one
authorized real step when either count is positive and records exit 3. An error
document or exit 3 from the real step is an alert and must never be retried.

For enablement, archive the exact original plist and its hash in operator-owned
scratch, install and hash the reviewed wrapper, apply the single diff and reload
the job only on Tom's go. Record the loaded job and hashes. Observe the next
seven scheduled runs (at least seven days), inspecting each retained packet and
launchd log for exits, candidates, triggered volumes, pending intents, stops
and unexpected effects. Stop the job and investigate on a nonzero exit,
missing packet, overlapping run, unknown state or unexpected candidate. No
automatic retries. This observation is evidence, not a claim that pressure
reclaim occurred.

Rollback requires the operator to unload the changed job, restore the exact
archived plist and reload it, then verify the loaded program path and compare
its hash to the saved original. Retain the wrapper and packets for evidence;
restoring the plist does not undo Barge effects. If needed, use the separate
kept-durability withdrawal in the [qualification procedure](t9-real-drive-qualification.md#withdrawal-and-report).
