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
gate only to be refused for its digest. The same file runs the per-case
preflight block described below against fixture inputs.

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
| Preflight sentinel | one child per case in the guarded session, alive for at most 2 s, then terminated and reaped inside the same 2 s |
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
- One preflight directory per case under the run root, holding only the sentinel
  identity file; the preflight probes write nothing else.

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
| 9 | The qualified runner bytes and selector are named and hash-pinned; Barge adds no second supervisor, session or accounting harness | the supervisor path and SHA-256 are constants in the script and in this plan; the script gates it as a readable regular Python file, runs it with `/usr/bin/python3` and refuses different bytes; before every case the preflight re-checks the pin and session, then makes the two required descriptor probes in the same guarded session and aborts the run if either is unavailable; the receipt records them; the two selectors are the whole entry point | startup supervisor pin, per-case preflight (sentinel and existing-process descriptor probes), `effigy cache:native`, `effigy cache:native:diagnostic` |

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
  was skipped or missing, the preflight ran before every recorded case with no
  failure, the real account state's before/after digests match, and the run
  exited 0; always false for the diagnostic twin;
- `preflight` — the probe names, the descriptor primitive, the guarded session,
  the sentinel's 2 s bound, how many preflights ran, the failure (or `null`),
  `status` (`passed`, or `infrastructure` on an abort), `aborted`, and one
  bounded observation row per probe per case: the sentinel
  and existing-process listings print `<kind> <uid> <descriptors> <vnodes>
  <identity-fd> <identity-vnode>`, and the supervisor row names the session and
  pin it re-checked;
- `mode`, `head`, `repo`, `binary`, `supervisor`, `supervisor_sha256`,
  `evidence`, the volume block (name, mount, backing, `size_mib`, `owners: true`),
  the `real_state` block (path, before, after, unchanged), the nine `premises`
  with their traces and evidence, the `cases` with status and evidence, and the
  `summary` (cases, recorded, passed, failed, skipped, not_run, first_fault,
  real_state_unchanged).

A receipt written by a failed run is still a receipt: it reports the first fault
and the cases it reached, or the preflight failure that aborted the run before
its cases. It never counts as a pass.

## The harness preflight

Every case that runs is preceded by one preflight of **two required descriptor
probes**, made through the same macOS primitive Barge's own observer uses:
`proc_pidinfo(PROC_PIDLISTFDS)` for the listing, and
`proc_pidinfo(PROC_PIDTBSDINFO)` for the *kinfo* fact that a process's effective
uid is this account's own. A valid supervisor pin and session say nothing about
whether this account can read descriptor evidence at all, so they are not
substitutes: they are only a supplemental check.

- **The sentinel probe** starts one sentinel child in this run's session — the
  shared supervisor's guarded session — as a plain child with no `setsid`, no
  `nohup` and no `disown`, so the supervisor's census still sees it. The child
  records its pid and session on an identity descriptor it keeps open, then
  execs a long sleep. While it is alive the probe reads its kinfo fact and lists
  its descriptors, requiring the live identity vnode on the recorded descriptor;
  then it terminates the child inside the same fixed 2 s bound, with a KILL only
  as an already-failed fallback, and reaps it with `wait`. The probe fails if the
  sentinel exited before its bound, if the listing is unavailable or does not
  show the live identity descriptor, if it ran in a different session, or if it
  survived the reap.
- **The existing-process probe** makes the same two calls on at least one
  pre-existing process of this account — this shell first, then its parent —
  after confirming the kinfo fact. A process whose effective uid is not this
  account's is skipped, never inspected.

When either descriptor-listing capability is unavailable, or either probe
otherwise fails, the run **records INFRASTRUCTURE, stays unqualified, and aborts
before any case body**: the case is recorded `skipped` with the probe named,
every remaining case is stopped in both modes, the receipt carries
`preflight.aborted: true` and the failure, and the run exits 125. A case is never
reported as a product failure when the harness itself is unsound, and a preflight
is never retried. The probes start no second supervisor, create no second
session, and write only inside the run root; the source test runs both their
success paths and their abort paths below the elevated tier.

## Diagnostic history

The first uncounted diagnostic ran at `4c1d6743cd67beb58c0c268ac1061a2b695b6d49`
and failed: five cases passed, `absent-path` and `empty-directory` failed,
`hand-made-in-place` failed on a missing `$HOME/.cargo`, and `ordinary-detach`
was skipped by its dependency. The guard closed cleanly (fault none, 11.1 MiB of
its 512 MiB budget, evidence kept), the image was detached, and the real account
state's before/after digests were equal. Its receipt is `barge.s6.native/1` with
`qualified: false`.

It found three things, all since closed:

- the same-uid observation counted the caller's own held descriptors, so custody
  refused; #011 (`0d9301b`) excludes the calling process by pid and start
  identity, and the empty-directory proof in `cache` names its own exclusion;
- the case bodies redirected the CLI's `--json` document, so those two refusals
  left the case log empty; every CLI call now goes through `barge_cli`, which
  echoes the invocation, the document and its stderr into the case log, and every
  case log is retained under `$OXIDE_PROOF_EVIDENCE/cases/` and named by the
  receipt;
- `hand-made-in-place` never created `$HOME/.cargo` before linking the registry.

The failed receipt is history, not a proof: a fresh diagnostic is required at the
new head, then the counted author and independent reviewer natives.

A later source review at `0e7926d` found the harness preflight was still only a
plan and that its proposed form could not establish the descriptor capability the
custody decisions depend on. The apparatus correction replaces it with the
two-probe, per-case preflight above: the sentinel descriptor listing and an
existing-process listing, both through
`proc_pidinfo(PROC_PIDLISTFDS)`/`proc_pidinfo(PROC_PIDTBSDINFO)`, both required
before every case, with an infrastructure abort when either fails. It changes no
selector, bound, case or product code, and no receipt exists yet at the new head.

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
