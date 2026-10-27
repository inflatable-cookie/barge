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
/usr/local/bin/barge maintain --json --plan-sha256 <hex> [--plan <file>]
```

The first form is the preview and is effect-free: it prints one bounded canonical
`barge.maintain.plan/1`, its SHA-256 in `data.plan_digest`, and the number of
units it would delete in `data.candidates`. `--plan-out` also writes exactly the
bytes the digest covers to the named file, for a caller that wants to keep them.

The second form is the real run and needs the preview's digest. `--plan <file>`,
when given, must hold exactly the plan bytes this head computes; a file from any
other state is refused before any effect.

## What the job does

1. Run the preview. Record its exit code.
2. **Only** when the preview exited `0` and `data.candidates` is greater than
   zero, run the real step once, passing that preview's `data.plan_digest`.
3. Record the real step's exit code. Never retry it, never loop, never run it
   twice, and never run it with a digest from an earlier day's preview.
4. Do nothing else. The job sweeps scratch and applies each declared area's prune
   policy; it never runs Oxide's outputs prune, never reads an Oxide receipt or
   journal, and never deletes an Oxide root.

A pending intent is finished by the next scheduled real run's own replay, not by
a retry inside the same night.

## Exit codes and what the job records

| Exit | Meaning for the job |
| --- | --- |
| `0` | Completed. Nothing else to do. |
| `2` | Refused before any effect: a usage error, a plan that is not the plan, an absent volume it named, a lock, a custody refusal. Record and stop for this schedule. |
| `3` | Unsettled or unknown state that needs a human: a pending intent, a replay that stopped short, the run's wall clock, a run that stopped on an error after its first effect (reported in `data.stopped`, never as a "refused before any effect" `2`), an absent **permanent** volume, or a volume whose durability is undeclared. Record and stop. |

A non-zero exit is recorded and never retried by Barge. The recording is the
host's: launchd's `StandardOutPath`/`StandardErrorPath` append one line per run,
and Barge writes nothing about the job into its own state.

An absent `ephemeral` volume is not a failure: it is reported in the plan with
`loud: false`, and the run exits `0`. An absent `permanent` volume is reported
with `loud: true` and forces exit `3`, because a drive that should be attached and
is not needs a human. In both cases the other volumes are still maintained and no
mount point is ever recreated.

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
