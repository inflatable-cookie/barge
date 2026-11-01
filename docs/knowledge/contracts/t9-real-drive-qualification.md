# Builds real-drive qualification

This is an operator procedure, not authority to execute it. Tom's explicit go
is required separately for registration and the reviewed, at-most-one prune.
The host-job change has a separate go. No guard, recheck, unregister, scratch
claim or delegation is part of this procedure. Withdrawal changes durability to
`kept`; it does not unregister. The run does not claim reclaim under pressure.

Before F6 Oxide declares no areas. Only Barge-owned scratch under
`/Volumes/Builds/barge/scratch` can participate; any candidate outside it is a
STOP. Oxide and Effigy entries must appear only as kept rows, never candidates.
`root_dir=barge` keeps this area distinct from the shared
`/Volumes/Builds/scratch`. Registration must show empty scratch grants.

## Common protocol

Run as the intended account on macOS with Builds mounted; unset
`BARGE_STATE_DIR` and `BARGE_ACCOUNT_HOME`. Do not run other Barge or host-job
operations concurrently. Record the UTC start, operator, main commit and each
command's exit in `notes.txt` in the packet. Every command below is sequential;
stop on nonzero exit, an unknown/truncated result, unexpected document schema,
identity change or a deadline, unless explicitly stated otherwise. Capture stdout
and stderr separately as shown. Do not retry a real prune. After every step run
`(cd "$E" && shasum -a 256 -- *.json *.txt 2>/dev/null > SHA256SUMS)` and
retain the packet. Hashes identify evidence, not permission to proceed. If a
command times out or an effect has uncertain state, stop and retain evidence;
do not kill or retry it. The Barge internal limits are 10 seconds for a space
plan, 120 seconds for prune and 600 seconds for maintenance; allow 30 seconds
for each other Barge command and 60 seconds for each observation. Exceeding a
limit is a stop for operator investigation. No instruction here removes data.

Build the binary from the reviewed T9a merge commit, with clean main and its
identity recorded. This is an Oxide build, not an installation:

```sh
git rev-parse HEAD
oxide build --release --locked -p barge-cli
B="$(pwd -P)/target/release/barge"
shasum -a 256 "$B"
python3 scripts/t9-preview.py --self-check
python3 scripts/t9-preview.py --binary "$B"
```

The script prints the fresh packet directory. Set `E` to that exact path and
record the commit and binary hash in `notes.txt`. It bounds each child to 60 s
and each output to 1 MiB, reads the mount UUID twice, records `diskutil`, `df`,
statvfs, Barge state and space, Oxide pool and the presence of the pool, outputs
and scratch, plus a registration preview and maintenance preview. It permits
only fixed report and registration-plan argument shapes. Review `failure.json`,
all `exit` values and `SHA256SUMS`; stop on any failure, missing output, wrong
UUID/mount or existing Builds registration. `describe.json`'s inner stdout is
`barge.describe/1`; `space.json` is `barge.space/1`. Check no existing consumer
areas or grants could authorize candidates. Record the UUID from `volume.json`
as `U`. Compare `statvfs.json` with `df.json`; the proposed reserve is the lesser
of 100 GiB and half measured available space, leaving headroom above the 10 GiB
floor. Stop if free is at most 20 GiB, thresholds exceed total, or the operator
cannot accept the headroom. The preview's register document is
`barge.volume.register/1`, with `data.plan` and `data.plan_sha256`.

The script is a fresh measurement, not a durable approval. Capture its exact
plan bytes and verify its digest, then record the registry digest and proposed
record, including `root_dir`, durability, policy and empty scratch grants:

```sh
python3 - "$E" <<'PY'
import hashlib, json, pathlib, sys
p = pathlib.Path(sys.argv[1]); outer = json.loads((p/'register_plan.json').read_text())
doc = json.loads(outer['stdout']); assert outer['exit'] == 0 and doc['schema'] == 'barge.volume.register/1' and doc['ok']
data = doc['data']; raw = data['plan'].encode(); assert hashlib.sha256(raw).hexdigest() == data['plan_sha256']
(p/'register.plan.json').write_bytes(raw)
print('plan SHA256', data['plan_sha256'], 'registry SHA256', json.loads(raw)['registry_sha256'])
PY
```

Capture this output in `notes.txt`, then set the values used below:

```sh
U=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["VolumeUUID"])' "$E/volume.json")
D=$(python3 -c 'import json,sys; print(json.loads(json.load(open(sys.argv[1]))["stdout"])["data"]["plan_sha256"])' "$E/register_plan.json")
```

STOP if any of those checks fail. No
rollback is needed before registration; discard no evidence.

## Registration and measurement

1. **Go point.** Present the packet and exact `register.plan.json` digest to Tom.
   Stop until Tom explicitly authorizes registration. Record the go and time.
2. Within 30 seconds after starting each command, apply the exact reviewed plan
   and describe it (substitute `U` and the printed plan digest, `D`):

   ```sh
   "$B" volume register --plan-file "$E/register.plan.json" --plan-sha256 "$D" --json >"$E/register.json" 2>"$E/register.err.txt"
   "$B" volume describe "$U" --json >"$E/registered.json" 2>"$E/registered.err.txt"
   "$B" space --json >"$E/space-after.json" 2>"$E/space-after.err.txt"
   /bin/df -k /Volumes/Builds >"$E/df-after.txt"
   ```

   Expect `barge.volume.register/1`, `barge.volume.describe/1` and
   `barge.space/1`, all `ok: true`; verify the UUID and that the described
   record round-trips the approved plan. Record the registry file SHA-256
   (`shasum -a 256 "$HOME/Library/Application Support/barge/state/registry.json"`)
   in `notes.txt`. Match the measured free and total against `df` in bytes within
   one filesystem block, allowing for measurement time; stop on mismatch. Record
   exits, timestamps and hashes using the common protocol. On any failure or to
   withdraw, stop and follow the withdrawal procedure below only after the
   required authorization; an uncertain registration is not assumed absent.
3. Preview the prune, saving the exact canonical bytes (10 s planning bound):

   ```sh
   "$B" space prune --volume "$U" --plan --plan-out "$E/prune.plan.json" --json >"$E/prune-preview.json" 2>"$E/prune-preview.err.txt"
   ```

   Expect `barge.space.prune/1`, `preview: true`, `effect: null`, an untruncated
   `barge.space.prune.plan/1`, and a `plan_sha256` equal to
   `shasum -a 256 "$E/prune.plan.json"`. Review every candidate and kept row.
   STOP on any outside Barge scratch, any Oxide/Effigy candidate, unknown identity,
   incomplete accounting or unexpected pending intent. Record and hash evidence.
   This plan may legitimately be empty because Builds is above reserve.
4. Capture pre-prune directory listings and their hashes using the exact command
   below. It records names and `lstat` identity, not contents, without following
   links. Each directory has a 100,000-entry cap; a missing, unreadable or
   changing directory is a STOP. Repeat after the prune with `PHASE=after`.

   ```sh
   PHASE=before python3 - "$E" <<'PY'
import json, os, pathlib, sys
out = {}; root = pathlib.Path('/Volumes/Builds')
for name in ('.', 'oxide-pool', 'oxide-outputs', 'scratch'):
    p = root/name; before = p.stat(); rows = []
    with os.scandir(p) as entries:
        for e in entries:
            if len(rows) >= 100000: raise RuntimeError('listing cap')
            s = e.stat(follow_symlinks=False); rows.append([e.name, s.st_mode, s.st_dev, s.st_ino])
    after = p.stat()
    if (before.st_ino, before.st_mtime_ns) != (after.st_ino, after.st_mtime_ns): raise RuntimeError('listing changed')
    out[name] = sorted(rows)
pathlib.Path(sys.argv[1], os.environ['PHASE']+'-listings.json').write_text(json.dumps(out, ensure_ascii=True, sort_keys=True)+'\n')
PY
   ```

   Record the exit, timestamp and hashes. Stop on any listing failure.
5. **Go point.** Present the preview, candidates, `prune.plan.json` digest and
   before listings to Tom. Stop until Tom explicitly authorizes this exact
   plan. Set `P=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["plan_sha256"])' "$E/prune-preview.json")`; verify it against the saved file and execute at most once (120 s internal bound):

   ```sh
   "$B" space prune --volume "$U" --plan "$E/prune.plan.json" --plan-sha256 "$P" --json >"$E/prune-real.json" 2>"$E/prune-real.err.txt"
   ```

   Expect `barge.space.prune/1` with `preview: false` and non-null `effect`.
   Record the exit, all replay, pruned, held, not-reached, stop and flush rows.
   Exit 3 or any uncertainty is a STOP, not a retry; replay may itself have
   effects. Repeat the listing command as `PHASE=after`, compare with the before
   listing and account for every change against the reviewed candidates. Any
   change outside planned candidates is a STOP; listing digests alone do not
   establish unchanged file contents. Retain both listings and hashes. Withdraw
   to kept on authorization if the operator stops the qualification.
6. Preview maintenance (600 s internal bound):

   ```sh
   "$B" maintain --space --json --plan-out "$E/maintain.plan.json" >"$E/maintain.json" 2>"$E/maintain.err.txt"
   ```

   Expect `barge.maintain/1`, `ok: true`, a `plan_digest` matching the saved
   bytes and a numeric `space_triggered`; record both. Exit 3, pending, unknown,
   truncation, or a candidate outside Barge scratch is a STOP. Capture exit,
   timestamp and hashes. This is a preview and does not run maintenance.

## Withdrawal and report

Plan a withdrawal (30 s bound) with `"$B" volume update "$U" --durability kept
--declared-by t9-qualification --plan --json >"$E/withdraw.json" 2>"$E/withdraw.err.txt"`.
Expect `barge.volume.update/1` with `data.plan` and `data.plan_sha256`; extract
and verify the bytes as for registration, using this command (the withdrawal document is direct JSON):

```sh
python3 - "$E" <<'PYCODE'
import hashlib, json, pathlib, sys
p = pathlib.Path(sys.argv[1]); doc = json.loads((p/'withdraw.json').read_bytes())
assert doc['schema'] == 'barge.volume.update/1' and doc['ok']
data = doc['data']; raw = data['plan'].encode()
assert hashlib.sha256(raw).hexdigest() == data['plan_sha256']
(p/'withdraw.plan.json').write_bytes(raw)
print(data['plan_sha256'], json.loads(raw)['registry_sha256'])
PYCODE
W=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["plan_sha256"])' "$E/withdraw.json")
```

Review the proposed kept record and registry digest.
Record and hash evidence. This rehearsal alone changes nothing. To demonstrate
withdrawal, obtain Tom's explicit go for this exact plan, then execute once
`"$B" volume update --plan-file "$E/withdraw.plan.json" --plan-sha256 "$W" --json >"$E/withdraw-applied.json" 2>"$E/withdraw-applied.err.txt"`,
where `W` is its digest. Run `"$B" volume describe "$U" --json >"$E/withdraw-describe.json" 2>"$E/withdraw-describe.err.txt"` and verify kept durability.
Both have 30 s bounds; any refusal or unknown is a STOP. Record exits and hashes.
There is no unregister; do not restore expendable durability without a new go.

Finish by recording pass/fail for each criterion, UUID, commit and binary hash,
registry digest, `df` comparison, plan digests, all candidates, before/after
listing differences, real prune effects, maintenance trigger, withdrawal and
any uncertainty in `report.txt`. Recompute `SHA256SUMS`; verify it with
`(cd "$E" && shasum -a 256 -c SHA256SUMS)`. Deliver the packet to Tom. A
missing result is not a pass. See [host-job hand-off](host-job.md) for the
separately gated change plan.
