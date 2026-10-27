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

Effigy owns three selectors. The two native ones each declare `admission =
"heavy"`, the same admission as S4's natives, and each is exactly one command —
the one fixed script:

```text
maintain:native              python3 scripts/native/s7-maintain-proof.py counted
maintain:native:diagnostic   python3 scripts/native/s7-maintain-proof.py diagnostic
maintain:native:test         python3 scripts/native/s7-maintain-proof.py --self-check
```

`scripts/native/s7-maintain-proof.py` is the whole command the elevated run
supervises, in both modes; the CLI under test is named by `BARGE_PROOF_BIN`, which
the owner builds at the head being proved. Its outer half refuses to run outside
that supervisor: it verifies the pinned bytes, then executes them with
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
and the script contains no force detach. It also parses the argv the outer half
hands the guard — `argument_parser().parse_args(inner[2:])` for both `counted` and
`diagnostic` — and requires `--inner <mode>` and `--binary <path>` to come back
unchanged, with the counterfactual that an executable in the positional `mode`
slot is refused. That guard exists because the first activated diagnostic at
`24c5018` failed exactly there; it is pure, so it runs before any native starts
without spawning a process or creating an image. It also holds the ordering the case
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
   run, authorised by that plan's exact bytes and digest, deletes exactly those
   two — the scratch stage first, then the area stage — leaves the fresh root and
   its marker in place, and reports nothing pending with exit `0`.
2. **`a_tampered_plan_refuses`.** `--plan-out` writes the exact preview bytes; the
   case proves their SHA-256 is the preview's digest and writes a *separate*
   tampered copy with one byte appended, so the preview's own file is never
   modified. The tampered copy with the preview's digest requires exit `2` with
   `usage` and "do not match", with the planned root still present, and the
   authentic file is re-read and proved unchanged. The preview's authentic bytes
   and digest then authorise the real run, which deletes the root that plan
   named.
3. **`a_hold_between_preview_and_real_stops_that_unit`.** Two aged roots, one
   preview. A hold is taken through `barge area hold` with the proof's own live
   pid, which the product reads as live. The real run authorised by that same
   first plan re-verifies each unit: the held root is kept and reported in the
   area stage's `skipped` rows with its hold reason, the other planned root is
   still deleted, nothing is pending, and the run exits `0`. A fresh preview then
   reports the held root in `kept`, so the next run does not plan it either.
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
6. **`exit_3_with_a_pending_intent`.** An owned root holding a symlink, with the
   entry created **before** the root is aged (adding an entry refreshes the
   directory mtime, so aging first would plan nothing — the arrangement defect the
   first `9a573a9` diagnostic exposed), and the symlink pointing at a fixture file
   inside the run root. The case asserts the preview names exactly that unit before
   the real run; the real run then exits `3` with exactly one pending intent, the
   intent record is not `deleted`, the area folder holds exactly one
   `.barge-prune-` leaf, the unit's visible name is gone, and the symlink's
   referent still holds its exact bytes. Nothing replays it: the image is discarded
   after the receipt.

### The ownership oracle

The brief requires the owned image to be attached with `-owners on`, and the attach
argv is asserted as part of the command the proof runs. This platform's plist form
does **not** report the result: on macOS 26.6.2 `diskutil info -plist <volume>`
carries no `Owners` key for any volume, so a plist read can never establish it. The
proof therefore binds the attached image's UUID, device, whole disk and mount from
the plist (as it always has) and then reads that **same volume's text form**,
`diskutil info <volume>`, for exactly one `Owners:` line whose value is `Enabled`.
A missing line, `Disabled`, two lines, or any other value is refused as a premise
failure — ownership is never inferred from the attach command alone. The answer is
recorded in the receipt as `image.ownership` (`attach_argv`,
`plist_keys_with_owner`, `text_state`), and `maintain:native:test` states the
oracle's counterfactuals for Enabled, Disabled, missing, ambiguous and unrecognised
text without spawning a process or touching a volume.

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

## Recorded diagnostics

None of these qualify a head. A diagnostic that fails is recorded here rather
than promoted, and every one needs a fresh, separately owner-activated run at the
head it is meant to describe.

### 2026-10-27 — head `9a573a9`, third owner-activated diagnostic: 5 of 6 cases passed, case 6 arrangement defect

- **Selector and mode:** `effigy maintain:native:diagnostic` (heavy admission) at
  `9a573a9d6595e4eabf3179fcc7b39de0fcc55fdf`; one invocation, no retry, exit `3`,
  `qualified: false` (an uncounted diagnostic is never counted PASS).
- **What executed:** the preflight census **passed** (16 451 descriptors, 335
  foreign-uid processes classified, 0 open under the root, OS blind spot: 8
  verified platform binaries with 67 opaque descriptors); the ownership oracle read
  the volume text form as `Enabled` with no plist `Owners` key; the image was
  attached `-owners on`, detached ordinarily (`exit 0, forced false`); the nine
  premises were traced with `premise_failure: null`; the real account state was
  observed absent and unchanged; the guard closed with `census.certain`, 0 members,
  8.0 MiB peak scratch of 512 MiB.
- **Passed:** `preview_then_real_deletes_exactly_the_planned_units`,
  `a_tampered_plan_refuses`, `a_hold_between_preview_and_real_stops_that_unit`,
  `an_oxide_output_root_and_receipt_fixture_are_untouched`,
  `an_absent_ephemeral_volume_is_skipped_and_a_permanent_one_is_reported`.
- **Failed (apparatus arrangement, not product):** `exit_3_with_a_pending_intent`
  exited `0` instead of `3` because the plan it was handed named **0 candidates**:
  the case created its symlink *after* aging the root, and adding an entry refreshes
  the directory mtime, which is an owned root's last activity. Nothing was planned,
  so nothing stopped.
- **Fix in this change:** the case creates the entry before aging the root, points
  the symlink at a fixture referent and requires it unchanged, and asserts the
  preview names exactly its unit before the real run. `maintain:native:test` gained
  a pure fixture-order guard (the symlink call must precede the aging call in that
  case, and the case must assert its planned candidate), verified to fail when the
  order is reversed.
- **Preserved packet (unchanged):** guard evidence
  `/Volumes/Builds/scratch/barge/oxide-proof.umacocyb/evidence/` — guard receipt
  `a763b17146da80ff754b67ef3b94619fb85897c4b856f9c1bc75fab69d867cba`,
  proof receipt
  `233cf71cc245c9d868dd05ce1b3f0597b4829f1a9e0fbe9eaac9d72f3944bcec`,
  stdout `b2963c97140820d224ec82b08d5f74f4ce49668f261390d7a71fefd29977c7e8`,
  stderr (empty) `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`;
  selector log `/Volumes/Builds/scratch/barge/run.JPI74V/diagnostic.log`
  `8b9765156e7a30c12c473305d32012fcea740a17469ec24cae048b150222a110`.

### 2026-10-27 — head `620ab6f`, second owner-activated diagnostic: setup premise refused, no case

- **Selector and mode:** `effigy maintain:native:diagnostic` (heavy admission) at
  `620ab6fb3205aaa70be05c5d446366751db65561`; one invocation, no retry.
- **Outcome:** exit `3`, `unqualified`, receipt present but no case evidence. The
  argv fix held: the guard ran `--inner diagnostic --binary <path>`, attached a
  64 MiB image with `-owners on` and detached it ordinarily
  (`detach: [{mount, exit: 0, forced: false}]`).
- **Cause (apparatus, not product):** `setup` refused with
  `kind: premise, detail: "the owned image did not enable ownership: None"` — the
  proof read `Owners` from `diskutil info -plist`, which this platform never
  reports. Second, smaller defect in the same receipt: `supervisor.path` recorded
  `…/scripts/scripts/bounded-test.py`, a doubled segment.
- **Fix in this change:** the ownership oracle now reads the volume's text form for
  an explicit `Owners: Enabled` (above), keeping the attach argv assertion and
  refusing everything else; the receipt records the verified supervisor path and no
  longer reconstructs it. No product rule changed.
- **Preserved packet (unchanged):** guard evidence
  `/Volumes/Builds/scratch/barge/oxide-proof.rn3q7pf_/evidence/` — guard receipt
  `1bd476fced2dab21341dbabe3bc1f9f308d3f15989942a2bd8a3d0610068f75c`,
  proof receipt
  `4acd4bee4ded5f09196aa417542c6da03f695cf9bf460087d6f94af1a6574855`,
  stdout `5bf50bdd68fb17c46746fb485ee1165c8161a22746ce197c6636253a48eb9305`,
  stderr (empty) `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`;
  selector log `/Volumes/Builds/scratch/barge/run.uNDYiG/diagnostic.log`
  `9de46d7a48cf1e7246384de654c321b977e9c701aa0268569b9a665401070589`.

### 2026-10-27 — head `24c5018`, first owner-activated diagnostic: failed before any case

- **Selector and mode:** `effigy maintain:native:diagnostic` (heavy admission) at
  `24c5018dbc15236fab46469157c5ed194fe18042`; one invocation, no retry.
- **Outcome:** exit `2` after 36 s. No case ran, no image was created and no
  premise was executed: the proof's own receipt `s7-maintain-receipt.json` does
  not exist, so nothing here is evidence for any pass gate.
- **Cause (apparatus, not product):** the outer half handed the guard
  `… --inner diagnostic <binary>`, putting the executable in the positional
  `mode` slot; the inner parser refused it (`argument mode: invalid choice`) and
  the script exited before `setup()`. The guard's own receipt records that argv
  verbatim, and it closed cleanly (`closure: closed`, `status 2`, `fault None`,
  `deadline False`, census `certain` with `0` members, owned-byte peak `4096` of
  `536870912` B).
- **Fix in this change:** the inner argv is
  `… --inner <mode> --binary <path>`, built by `inner_command()` and
  `supervisor_command()`, and `self_check` now parses it for both modes (see
  above). No product rule changed.
- **Preserved packet (unchanged):** guard evidence
  `/Volumes/Builds/scratch/barge/oxide-proof.033ykvxz/evidence/` — `receipt.json`
  `8155d637f353071b4ac757b3d235edfae63892d1f2158005a01a988b1a89659e`,
  `stderr.log` `8be7038b609e64a956dd10e22c13b4fc2328f7f605b0b7d56be8fb074b36f315`,
  `stdout.log` (empty) `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`;
  selector log `/Volumes/Builds/scratch/barge/run.yjEpaa/diagnostic.log`
  `9cd2d60ffa0ca0de57609457b83a947a16328a025936d0c7706a32f9a9fa0849`.

## What this proof does not claim

No real-drive maintain run, no LaunchAgent or reaper change, no Oxide or Effigy
adoption, no qualification of a schedule, and no count of the reaper's own
behaviour. The handover comparison against the reaper's dry run needs a real
drive and stays an operator step; the [host-job contract](host-job.md) owns that
half.
