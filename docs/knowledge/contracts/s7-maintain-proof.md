# S7 native proof: `barge maintain`, preview and real

Native qualification is owner-gated. One fixed script and three Effigy selectors
let the owner activate an elevated run at one exact head. No receipt exists yet;
a missing receipt is a missing proof.

The design's elevation path is [in the stage 1
design](stage1-design.md#high-premise-checklist-and-elevation-path): a HIGH
premise record, then one uncounted diagnostic before any counted run, then a
separate counted author run and a separate independent reviewer run, each
separately activated and each at an exact head. This is the premise record and
the case bodies for S7 ([the S7
row](stage1-design.md#implementation-slices-and-dependencies)). It changes no
product rule: every deletion it provokes is S4's grant-checked sweep or S5's
custody and delete path, on roots whose `barge.root/1` markers Barge itself
wrote.

## The fixed command

Effigy owns three selectors. The two native ones are exactly two commands each —
the CLI built from the head under proof, then the one fixed script:

```text
maintain:native              oxide build -p barge-cli --locked
                            python3 scripts/native/s7-maintain-proof.py counted

maintain:native:diagnostic   oxide build -p barge-cli --locked
                            python3 scripts/native/s7-maintain-proof.py diagnostic

maintain:native:test         python3 scripts/native/s7-maintain-proof.py --self-check
```

`scripts/native/s7-maintain-proof.py` is the whole command the elevated run
supervises, in both modes, and its outer half refuses to run outside that
supervisor: it verifies the pinned bytes, then executes them with
`/usr/bin/python3`, and the inner half refuses without `OXIDE_PROOF_EVIDENCE` and
a `TMPDIR` inside `OXIDE_TASK_SCRATCH_ROOT`.

### The supervisor, pinned

Barge writes no second supervisor, session or accounting harness. The script
verifies and then executes Oxide's qualified shared supervisor:

| Fact | Value |
| --- | --- |
| Path | `~/Dev/projects/oxide/scripts/bounded-test.py` |
| SHA-256 | `bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924` |
| Invocation | `/usr/bin/python3 <supervisor> --limit 300 --scratch-budget-mib 512 --root-parent /Volumes/Builds/scratch/barge -- <script> --inner <mode> <binary>` |
| Mode | a Python file Oxide keeps mode `0644`: a readable regular file is the gate and the digest is the identity; the execute bit is never required and its mode is never changed |
| Refusal | an unreadable or non-regular file, different bytes, a missing CLI or a missing designated scratch parent → exit `125`, nothing run |

`maintain:native:test` proves the same pins without an image: the case map
resolves every case name to a `case_<name>` method, no case method is
undispatched, each dependency is evaluated first, the nine premises are recorded
and traced, the environment inventory covers every hook the source reads or sets,
and the script contains no force detach. It also holds the ordering the case
bodies need — the planned-deletion cases before the absent-volume case, the
pending-intent case last — that the absent-volume records are registered by that
one case, and that it restores the one-volume registry in a `finally` block, so a
future edit cannot quietly make the earlier cases observe a loud plan or leave a
pending intent for a later case's real run to replay.

## Resources

| Resource | Bound |
| --- | --- |
| Wall clock | 300 s (the supervisor's own deadline; it reserves its stop window) |
| Owned scratch | 512 MiB monitored by the supervisor |
| Owned APFS image | one sparse image, 64 MiB, `hdiutil ... -owners on` |
| Evidence | `$OXIDE_PROOF_EVIDENCE/s7-maintain-receipt.json`, never removed by cleanup |
| Guard root parent | explicit `/Volumes/Builds/scratch/barge`; the supervisor creates its own owned root below it |
| Run root | one `mktemp -d` under the guard's owned scratch; every fixture directory is a `mkdtemp` inside it |
| Detach | ordinary only; this script has no force path, and a failed detach keeps the image and its backing and makes the receipt unqualified |
| Preflight | one `barge diagnostic descriptor-census --root <mount>`: `passed` continues, anything else is `infrastructure` (exit 4) before any case |

One image is enough for the brief's "up to two": the two absent volumes case 4
needs are registered records whose mount points are never created. That proves
the design's rule — an absent volume's mount point is never recreated — more
directly than a second attached image would.

## What the run creates

- One owned APFS sparse image, created empty, attached with ownership enabled at
  a unique `/Volumes/BargeMaintainProof<pid>-<tick>`.
- A private `HOME`, `CARGO_HOME` and state directory inside the run root, so no
  real account state is read or written.
- One fixture `barge.registry/1` that starts with the owned image's volume only
  (permanent, expendable). The absent-volume case adds two further explicit
  records — a permanent and an ephemeral one whose `mount_hint` paths are *inside
  the run root and never created* — and restores the one-volume registry
  afterwards. A record for a volume that is not attached makes every plan loud
  (exit `3`), so registering those two first would stop every planned-deletion
  case from observing the quiet exit its behaviour is stated against.
- One area declaration, `example/pool`, with `--absent wait`,
  `--prune oldest-idle:60` and `--reserve counts`, written by the area verb.
- Owned roots through `barge area root create`, one native scratch run through
  `barge scratch alloc`/`conclude`, and one hold through `barge area hold`.
- Nothing else: no real drive, no LaunchAgent, no reaper, no Oxide state.

## The nine premises, traced through the case bodies

Each premise is carried by a body the proof actually runs, and the receipt
carries the same trace (`premise_trace`).

| # | Premise | Body that carries it |
| --- | --- | --- |
| 1 | a private `HOME`, `CARGO_HOME` and state directory | `setup`: the child environment sets all three inside the run root |
| 2 | the fixture account home, and the real account state untouched | `setup` observes `~/Library/Application Support/barge/state` before and after with a bounded, `O_NOFOLLOW` walk that hashes every entry's name, kind, mode, size, mtime and bytes; `child_environment_problems` proves the child's `BARGE_STATE_DIR` and `HOME`; the receipt reports both observations and refuses the run if either is incomplete or they differ |
| 3 | environment isolation | `setup`: `TMPDIR`, `TMP` and `TEMP` are the run root, `BARGE_STATE_DIR` the fixture state, and `DYLD_INSERT_LIBRARIES` is discarded |
| 4 | explicit adoption only | `register` and `declare_area` write the registry and the declaration by command; every root is created by a command |
| 5 | one owned image with `-owners on`, ordinary detach | `create_image` and `cleanup`: attach options are asserted by the self-check, and the only detach is `hdiutil detach <mount>` after re-reading that the mount is still the owned UUID and device |
| 6 | every environment hook listed | `ENVIRONMENT` names all ten, and `maintain:native:test` fails on an unlisted or unused hook |
| 7 | temporary storage inside the owned root | `setup`: every directory is created with `dir=` the run root, which is inside `TMPDIR` |
| 8 | `mktemp -d` per fixture, never a shared name | `tempfile.mkdtemp` for the run root's children; no fixed path anywhere |
| 9 | the pinned supervisor, no second harness | `outer_half` verifies the digest and executes those bytes; the self-check pins it |

## The six cases

Each case asserts the design's observable and fails on a missing fact. The
counted mode stops at the first fault; the diagnostic keeps going, latches the
first fault, and skips only the cases that depend on a failed one.

The order is the one the case bodies need, not the order the brief lists them in:
the four cases that plan deletions run first against the quiet one-volume
registry, the absent-volume case then adds its two records and restores them, and
the pending-intent case runs last because it leaves an intent that any later real
run would replay.

1. **`preview_then_real_deletes_exactly_the_planned_units`.** One aged owned
   area root, one fresh one, and one concluded-then-aged native scratch run. The
   preview names exactly the two eligible units, lists the fresh root in `kept`
   with an age reason, and reports `candidates` equal to what it listed. The real
   run, authorised by that digest, deletes exactly those two — the scratch stage
   first, then the area stage — leaves the fresh root and its marker in place,
   and reports nothing pending with exit `0`.
2. **`a_tampered_plan_refuses`.** `--plan-out` writes the exact preview bytes; the
   case proves their SHA-256 is the preview's digest, appends one byte, and
   requires exit `2` with `usage` and "do not match", with the planned root still
   present. The same digest without a plan file then authorises a real run and
   deletes the root, which shows the digest is the authorisation and the file is
   the stricter reading.
3. **`a_hold_between_preview_and_real_stops_that_unit`.** Two aged roots, one
   preview. A hold is taken through `barge area hold` with the proof's own live
   pid, which the product reads as live. The real run authorised by the *first*
   digest refuses as a whole (exit `3`, `unknown_state`, both roots kept) — the
   conservative reading of a changed plan. A fresh preview then lists the held
   root in `kept` with its hold reason and the other root as a candidate, and its
   real run deletes only the unheld root while the held one survives.
4. **`an_oxide_output_root_and_receipt_fixture_are_untouched`.** The case plants
   an `oxide-outputs/proof-root/` tree on the volume and a receipt file in the
   fixture state, snapshots both (names, modes, sizes, mtimes, bytes), runs a real
   maintain that deletes its own planned unit, and requires both snapshots
   byte-identical. `maintain` never runs Oxide's outputs prune, never reads a
   receipt and never deletes an Oxide root.
5. **`an_absent_ephemeral_volume_is_skipped_and_a_permanent_one_is_reported`.**
   The case writes the two absent records itself. The plan reports the ephemeral
   record `absent` with `loud: false`, the permanent record `absent` with
   `loud: true`, and neither mount point exists afterwards. The preview and the
   real run both exit `3`; the real run still deletes the attached volume's
   planned root, so the other volumes are maintained. The one-volume registry is
   restored in a `finally` block, so the last case's plan is quiet again.
6. **`exit_3_with_a_pending_intent`.** An aged owned root holding a symlink. The
   real run exits `3` with exactly one pending intent, the intent record is not
   `deleted`, the area folder holds exactly one `.barge-prune-` leaf, and the
   unit's visible name is gone. Nothing replays it: the image is discarded after
   the receipt.

## The receipt

`s7-maintain-receipt.json` in the supervisor's evidence directory. It records
the mode, `qualified`, the classification (`pass`, `unqualified` or
`infrastructure`), the supervisor's path and pinned digest, the binary's path and
SHA-256, the image's backing, mount, UUID, device, whole disk and ordinary detach
outcome, the environment inventory and the child-environment check, the nine
premises and their trace, the preflight census, the real-state before/after
observations, each case's status with its evidence, any infrastructure
classification, any latched fault, and any cleanup failure. `qualified` is true
only for a counted run in which all six cases passed, no case was skipped, no
fault was latched, cleanup completed, and the real-state premise held.

## Activation order

1. The owner runs `maintain:native:diagnostic` once at an exact head.
2. The owner runs `maintain:native` (counted) at the same head, separately
   activated.
3. An independent reviewer activates both again at the exact merge head; its
   counted receipt is the independent pass the merge hold names.

Each run is separately activated, and the diagnostics of a failed run are
recorded below rather than promoted. A run whose classification is
`infrastructure` (the product's own descriptor census could not complete) is a
host capability result, not a case result, and it never qualifies a head.

## What this proof does not claim

No real-drive maintain run, no LaunchAgent or reaper change, no Oxide or Effigy
adoption, no qualification of a schedule, and no count of the reaper's own
behaviour. The handover comparison against the reaper's dry run needs a real
drive and stays an operator step; the [host-job contract](host-job.md) owns that
half.
