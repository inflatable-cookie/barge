# S6 native proof: plan and case bodies

Status: **plan only, not executed.** The native proof is owner-gated: this
document, the fixed script it names and the two Effigy selectors exist so the
owner can activate the elevated run at one exact head. No receipt exists yet,
and a missing receipt is a missing proof.

The design's elevation path is [in the stage 1
design](stage1-design.md#high-premise-checklist-and-elevation-path): a HIGH
premise record, then one uncounted diagnostic before any counted run, then a
separate counted author run and a separate independent reviewer run, each
separately activated and each at an exact head. This is the premise record and
the case bodies for S6 ([the S6
row](stage1-design.md#implementation-slices-and-dependencies)).

## The fixed command

Effigy owns two selectors, and each is exactly two commands — the CLI built from
the head under proof, then the fixed script:

```text
cache:native              oxide build -p barge-cli --locked
                          scripts/native/s6-cache-placement-proof.sh counted

cache:native:diagnostic   oxide build -p barge-cli --locked
                          scripts/native/s6-cache-placement-proof.sh diagnostic
```

`scripts/native/s6-cache-placement-proof.sh` is the whole command the elevated
run supervises, in both modes. It refuses to run without the supervisor's own
contract environment (`OXIDE_PROOF_EVIDENCE` and an `OXIDE_TASK_SCRATCH_ROOT`
that contains `TMPDIR`), so it cannot be run by accident below the tier.

### The supervisor, pinned

Barge writes no second supervisor, no session and no accounting harness. The
script verifies and then executes Oxide's qualified shared supervisor:

| Fact | Value |
| --- | --- |
| Path | `~/Dev/projects/oxide/scripts/bounded-test.py` |
| SHA-256 | `bc24b238a2ec6f0a68719f59b14bce2d69df0e30c2a351366fdab07db4975924` |
| Bytes checked | 3397 lines at the head this plan was written against |
| Invocation | `/usr/bin/python3 <supervisor> --limit 300 --scratch-budget-mib 512 -- <script> <mode> <binary>` |
| Mode | any: it is a Python file Oxide keeps mode `0644`, so a readable regular file is the gate and the digest is the identity; the execute bit is never required and its mode is never changed |
| Refusal | an unreadable or non-regular file, different bytes, or a missing CLI → exit `125`, nothing run |

A source test (`crates/barge/tests/native_proof_harness.rs`) holds the script to
this contract: the pinned digest is an exact constant, the supervisor gate is
`-f`/`-r` and not `-x`, the bytes are executed by `/usr/bin/python3`, the script
never changes a mode, and a fixture supervisor kept `0644` gets past the mode
gate only to be refused for its digest.

The receipt records the supervisor's path and the pinned digest it verified, and
the digest is compared before the supervisor is executed.

### Resources

| Resource | Bound |
| --- | --- |
| Wall clock | 300 s (the supervisor's own deadline; it reserves its stop window) |
| Owned scratch | 512 MiB monitored by the supervisor |
| Owned APFS image | one sparse image, 128 MiB, `hdiutil ... -owners on` |
| Evidence | `$OXIDE_PROOF_EVIDENCE/s6-cache-placement-receipt.json`, never removed by cleanup |
| Run root | one `mktemp -d "$TMPDIR/barge-s6-run.XXXXXX"` inside the guard's owned scratch |
| Detach | ordinary; a forced detach is allowed only for this image's own whole-disk device, and a failed detach keeps the backing file |

## What the run creates

- One owned APFS sparse image, created empty, attached with ownership enabled at
  a unique `/Volumes/BargeProof<hex>`; every case's `caches/` folder lives on it
  under a case-scoped `root_dir`.
- A private `HOME`, `CARGO_HOME`, state directory and account home inside the run
  root, one set per case (`cases/<case-id>/{home,state,out}`), so no two cases
  share a settings file, a registry or a placement record.
- One fixture `barge.registry/1` per case, in that case's private state
  directory, registering the image at the exact volume UUID the kernel reports.

Nothing is created, written or deleted outside the run root, the image's own
mount, and the evidence directory.

## The nine HIGH premises, traced through the case bodies

| # | Premise | How the run establishes it | Traced through |
| --- | --- | --- | --- |
| 1 | A private `HOME`, `CARGO_HOME` and state directory, so no real account state is read or written | every case exports `HOME`, `CARGO_HOME`, `BARGE_STATE_DIR` and `BARGE_ACCOUNT_HOME` inside `cases/<id>/`; the receipt records all four | all nine cases |
| 2 | The account home used for state resolution is the fixture's, and the real account state is untouched (before/after digest) | `BARGE_ACCOUNT_HOME` is the fixture home, so the CLI's account facts and `describe.state.home` name it (`plan-effect-free` asserts `describe` reports the fixture home and the fixture state directory); the real state directory is digested with `digest_tree` before the first case and again in cleanup, and the receipt holds both digests and `unchanged` | `plan-effect-free` (the `describe` assertion), all cases, the receipt's `real_state` block |
| 3 | Environment isolation: `BARGE_STATE_DIR`, `TMPDIR`/`TMP`/`TEMP` and every inherited scratch point inside the guard's owned scratch; no inherited real paths | the guard sets `OXIDE_TASK_SCRATCH_ROOT`, `TMPDIR`, `TMP` and `TEMP` inside its owned root; the script asserts `TMPDIR` is under `OXIDE_TASK_SCRATCH_ROOT` before the first fixture and refuses otherwise; `CARGO_TARGET_DIR` is unset so no inherited build path is used | guarded startup assertion, all cases |
| 4 | Explicit adoption only: every fixture placement is adopted with an explicit command and a recorded plan, never by an implicit default | every adopt is an explicit `barge cache adopt --kind … --volume …`; the in-place form additionally requires `--adopt-existing-root`; the plan case proves a plan writes nothing and refuses to adopt silently | `plan-effect-free`, `absent-path`, `empty-directory`, `setting-unchanged`, `hand-made-in-place` |
| 5 | Owned image with ownership enabled (`-owners on`), ordinary detach, never a forced detach of anything but the owned image's own whole-disk device | `hdiutil attach -owners on -mountpoint <unique /Volumes/…>`; `detach_image` tries the ordinary detach first and reaches for `-force` only on the whole-disk device `diskutil` reports as the image's own; a failed detach keeps the backing and fails the case | startup attach, `ordinary-detach`, cleanup |
| 6 | Every flag and environment hook used is listed with its purpose; no debug-only product bypass exists | the receipt's `premises[6].evidence` lists every hook and every product flag; the product flags are `--json`, `--kind`, `--volume`, `--placement`, `--write-setting`, `--adopt-existing-root`, `--plan`, and the product has no debug or force bypass | receipt `flags_and_hooks` |
| 7 | Temporary storage: everything inside the guard's one owned root; no home folder, no drive root, no fixed `/tmp/<name>` | the run root is `mktemp -d "$TMPDIR/…"`; the image backing, evidence-adjacent metadata, per-case homes and state all live under it; cleanup removes it only after an ordinary detach | run root, all cases, cleanup |
| 8 | `mktemp -d` under that root for every fixture directory, never a shared name | one `mktemp -d` run root, then `cases/<case-id>/{home,state,out}` created fresh per case under it; no fixed name is used anywhere | run root, `run_case` |
| 9 | The qualified runner bytes and selector are named and hash-pinned; Barge adds no second supervisor, session or accounting harness | the supervisor path and SHA-256 are constants in the script and in this plan; the script gates it as a readable regular Python file, runs it with `/usr/bin/python3` and refuses different bytes; the receipt records them; the two selectors are the whole entry point | startup supervisor pin, `effigy cache:native`, `effigy cache:native:diagnostic` |

`BARGE_ACCOUNT_HOME` is the one new environment hook S6 needed for this plan: it
is the fixture-only sibling of `BARGE_STATE_DIR`, documented in
[the design](stage1-design.md#implementation-status), consulted only when set,
and a normal run leaves it unset. Without it a proof cannot move the account
home, which the design fixes to the password database rather than `$HOME`.

## The case bodies

Each case runs in its own private home, state directory and case-scoped
`root_dir` on the owned image. `barge` is the CLI built from the same head.

| Case | Setup | Assertions | Design row it proves |
| --- | --- | --- | --- |
| `absent-path` | `~/.cargo/` exists, `~/.cargo/registry` is absent | adopt exits 0 with `form: link`, `restore: absent`; the link's text is the recorded target and is under the case's `caches/`; `cache list` reports `link: linked`; undo removes the link, leaves the path absent and keeps the owned root | "an absent path gets a link through a temporary name"; undo of a link |
| `empty-directory` | `~/.cargo/git` is an empty real directory | adopt installs a link with `restore: empty_directory`; undo restores a real, empty directory | "an empty real directory becomes a link and undo restores it empty"; the empty-directory custody proof |
| `setting-unchanged` | `~/.npmrc` already sets `cache=` to the case's `caches/npm` | adopt records `form: setting`, `changed: false`; the file's digest is unchanged; no backup is written; undo changes no byte | "a setting already pointing at the owned root changes no byte" |
| `setting-written` | `~/.npmrc` sets `cache=/old/cache` and an unrelated line | adopt without `--write-setting` would refuse (covered by the unit suite); with it, `changed: true`, the exact new value is in the file, the unrelated line survives, and the report names the change; undo restores the original digest exactly | "tool settings are written only during an explicit adopt, the exact change is reported, and undo reverts it" |
| `diverged-undo` | a written setting, then the file is edited by hand | undo exits 3 with `unknown_state`, the edited file's digest is unchanged and the record is still listed | "a file that diverged after adopt refuses undo" |
| `non-empty-refused` | `~/.cargo/registry` is a real directory with a payload | adopt exits 2 with `refused_boundary`, `changed: false` and a plan of at least two options; the directory's tree digest is identical; nothing became a link; no record exists | "a non-empty real directory refuses with a plan, and nothing is renamed or deleted" |
| `hand-made-in-place` | today's layout: `caches/cargo/registry` holds data and `~/.cargo/registry` is a symlink to it | adopt with `--adopt-existing-root` records `form: held_root`, whose publication link and target are the original path and the adopted directory; the tree digest ignoring `.barge-root.json` is identical; the link text is unchanged; the marker exists; undo removes only the marker and leaves the data and the link byte-identical | "in-place adoption of today's hand-made placements moves nothing"; held-root undo |
| `plan-effect-free` | `~/.cargo/` exists, nothing adopted | `describe` reports the fixture account home and the fixture state directory; `--plan` exits 0 with `plan: true`, `placement: null` and reported changes; the case's `caches/` folder was not created, the tool's path was not created, and no record exists | "`--plan` effect-free", the fixture account home |
| `ordinary-detach` | one adopted link, then an ordinary `hdiutil detach` | with the volume away, undo and adopt both exit 2 with `volume_absent`, the detail's `attach` and the message name `/Volumes/BargeProof…`, and the published link is unchanged; after an ordinary re-attach, `cache list` exits 0 and reports `link: linked` | Q-001: a cache fails clearly naming the drive, and works again after re-attach |

Dependency: `diverged-undo` depends on `setting-written`, and `ordinary-detach`
depends on `absent-path`. In diagnostic mode a failed dependency skips the
dependent case and records it as `skipped`; in counted mode the first fault stops
the remaining cases, which are recorded `skipped` with the stopping case named.

## The receipt

The script always writes `$OXIDE_PROOF_EVIDENCE/s6-cache-placement-receipt.json`
(`barge.s6.native/1`) and prints one compact JSON summary line. It holds:

- `qualified` — true only for a counted run in which all nine cases passed, none
  was skipped or missing, the real account state's before/after digests match,
  and the run exited 0; always false for the diagnostic twin;
- `mode`, `head`, `repo`, `binary`, `supervisor`, `supervisor_sha256`,
  `evidence`, the volume block (name, mount, backing, `size_mib`, `owners: true`),
  the `real_state` block (path, before, after, unchanged), the nine `premises`
  with their traces and evidence, the `cases` with status and evidence, and the
  `summary` (cases, recorded, passed, failed, skipped, not_run, first_fault,
  real_state_unchanged).

A receipt written by a failed run is still a receipt: it reports the first fault
and the cases it reached. It never counts as a pass.

## The harness preflight

Before any native case runs, one preflight proves the harness itself: the pinned
supervisor is executed once with `/usr/bin/python3` around `/usr/bin/true`, with
the same 300 s and 512 MiB bounds, from a fresh disposable root under
`/Volumes/Builds/scratch/barge`. Its evidence is the supervisor's closure and
cleanup record for that root, and only the positively owned empty scratch is
removed. The preflight runs no image, no proof case and no selector, and it is
sequenced after the harness correction's review. A preflight that is uncertain
about custody stops, and it is never retried.

## Activation order

1. **Uncounted diagnostic** — `effigy cache:native:diagnostic` at head `H`.
   Keep-going, latched first fault, dependent-skip, `qualified: false`.
2. **Counted author native** — `effigy cache:native` at the same `H`, after the
   diagnostic's findings are resolved.
3. **Independent reviewer native** — the reviewer's own `effigy cache:native` at
   the same `H`, separately activated.

Each step is separately activated by the owner, and each receipt names the head
it ran at. A receipt from a different head proves nothing about this one.

## Not claimed

- No real account setting, link, cache, scratch root or drive is adopted,
  written or deleted; the run below the tier is not authorized.
- No surprise removal of a real USB or Thunderbolt drive, and no measurement of
  a real drive's flush cost.
- No qualification of Oxide's or Effigy's own adoption, and no change to Oxide.
- The image's own detach and flush cost is not a model of a real unplug.
