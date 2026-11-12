# A controlled macOS proof execution context

This file owns the durable design of the execution context Barge's native proofs
run in: which account, host, volume and scratch root, under which authority, and
what must still be decided before anything is provisioned. It is preparation
only. Nothing here provisions an account, machine, VM, volume or setting, runs a
proof, inspects a retained root, installs or adopts anything, and nothing here
is availability, native, closure or adoption credit. Every declaration
activates nothing: each provisioning step and each proof occurrence needs its
own prospective authorization.

The operator approved preparing this bounded design on 2026-11-12 (operator
board decision `4219d3e8-6ba2-4175-92c5-df62c736594c` v2). That approval covers
the design and its review, not any action it describes.

## Why the execution context is the variable

Barge's writer custody is, by design, *cooperative plus same-uid observation*
([stage 1 design](stage1-design.md#writer-custody-and-safe-deletion)): before a
move or a delete it reads the kernel's process table and then every vnode
descriptor of every process whose effective uid is its own, across the whole
host, within 4096 processes, 65 536 descriptors, 10 s per scan and the bounded
re-observation rules of Q-010 and barge#012. What it can decide about those
processes is fixed:

- a process of another effective uid is classified and reported, never
  inspected, and does not block (Q-007, Tom's ruling);
- a hidden vnode descriptor is a counted, reported OS blind spot **only** for a
  process that its own code-signing status verifies as an Apple platform binary
  with an unchanged kernel start identity; every other hidden descriptor stays
  `Unknown` and the keeper refuses (Q-008, Tom's ruling `e528a712`);
- an unreadable effective uid, a moving start identity and a reached scan bound
  are `Unknown`, never absent or idle.

So a positive eligibility verdict needs a complete, empty same-uid observation.
Every other process the caller's account happens to be running is inside that
observation. A private `HOME`, a private `CARGO_HOME`, a private `BARGE_STATE_DIR`
and a synthetic account home are *data* isolation: they change what the fixture
reads and writes, and they are premise-bound by the nine-item checklist, but
they do not change who shares the caller's uid. **A private home is not process
isolation.**

The retained barge#061 facts point at the descriptor class and at nothing more,
and each bound identity keeps its own recorded result at the bound head
`6d88ee2`/tree `9860345c`:

- the adequate uncounted diagnostic `27339d0c`/`6823896179` passed all sixteen
  rows with no first fault, skip or unexecuted row; its own image
  `DB8B16B1-52BE-43C9-B4F3-C6E725DBC145` closed positively (ordinary detach exit
  0, reaped, mount absent, readable post-census `backing_attached=false`,
  `closed=true`); and its separate group closure was positive — one started and
  completed census round, `joined=true`, with both the deciding and the final
  census certain, `members=0` and `uncertain=0`. Its disclosure that the raw
  census prefixes are truncated and the full census bytes are not retained for
  independent re-parsing is a retention limit, not an uncertain attach or
  closure verdict;
- the failed author `20cf43a4`/`6823148735` retains a failed R1 with an
  insufficient deciding payload, R2–R16 unexecuted, and the open image
  `94838906-EC5B-4DB6-AD01-A97077D370A7`; its joined deciding and final
  certain-empty group closure was accepted separately from the failed count and
  image, and its cause and partial effects remain unknown;
- the failed author `68af0f27`/`6824114210` retains a closed-descriptor R6
  refusal and the open image
  `B5E838A9-A661-4998-8B93-133B4EAC86D8`;
- the open image `7E9A11B8-FA79-4CD0-8080-0C7D038C6F42` stays a retained
  obligation, and nothing here reconstructs a cause or a present detach binding
  for it.

So the retained evidence is one product descriptor-policy refusal, one count
whose deciding payload and image are failed or open, and one uncounted
diagnostic whose rows and both closures were recorded positively and which the
disposition still accepted only as uncounted. None of that establishes an
apparatus defect or an attributable host-transient cause.

The deployed qualified default's own census round that never completed before a
fixed window is a different identity with its own result: the lane's
proof-supervision assessment, which names no barge#061 occurrence and keeps its
blocked-versus-slow boundary unknown. The bound diagnostic's completed round
neither reproduces nor explains it, no observation of that default follows here,
and no retained root is inspected.

The guard's own closure is a *different* observation and stays session-scoped:
the qualified supervisor's census settles only over its cooperative session and
previously tracked identities, and an escaped, untracked child can be classified
foreign. Controlling the account does not make that census complete; it removes
the unrelated same-uid processes from the product's scan, which is the part of
that dependency a shared account cannot bound. No claim is made that a quieter
context would have prevented the historical stall.

## Fixed premises (none of these move)

The design is bounded by the shipped proof contracts; it may name a difference
for Tom to select, never relax one silently.

| Premise | Fixed value |
| --- | --- |
| Supervisor | Oxide's shared qualified supervisor, exactly `fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b`, invoked as `/usr/bin/python3 <supervisor> --limit 300 --scratch-budget-mib 512 --root-parent <parent> -- <script> <mode>` (600 for the listing proof); no substitution, wrapper or second custodian. Barge writes no second supervisor or daemon, and no promotion is authorized. |
| Scratch route | canonical `/Volumes/Stash/scratch/barge`, owner-only, this account's, on the APFS volume whose UUID is `8A660440-C919-45C6-A3F1-3365B9BD2006` mounted at `/Volumes/Stash`; no drive root, no home folder, no symlinked component, no other volume |
| Staging | installed toolchain `1.97.1`, `--locked` and offline, no auto-install, one fresh stage inside the guarded occurrence through ordinary Oxide; the artifact and its manifest are re-verified before any effect, and no earlier or shared binary is accepted |
| Feature | `proof-synthetic-account` (release) or `proof-descriptor-listing` (listing); the ordinary production build names neither |
| Nine premises | Oxide's read-only proof-authoring premise checklist, each with its executable disposition (`premise_dispositions()`), unchanged |
| Cases | the sixteen rows of the release proof at the bound source head `6d88ee2e31d2f0611b7001791cdc3a706e1bed5b` (R1–R16: R3's held writer, R2's missing identity record, R9's changed-digest refusal, R13–R15's registry facts and R16's paired control handoff), and the listing proof's L1–L4 |
| Bounds | release: 300 s total, 512 MiB monitored scratch, one 64 MiB sparse APFS image, the original 30 s work stop; listing: 600 s, 512 MiB, no image. Oxide#304's 600 s / 512 MiB / 128 MiB integrated gates stay separate. |
| Exclusion | none. No parent or driver process is excluded from the scan, no scan set is narrowed, no descriptor is skipped or reclassified, no retry or bound increase is added, and no new liveness policy exists. |

## Options

| # | Context | Same-uid control | Stash identity | Admission authenticity | Cost and residue | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| A | The shared developer account on the developer host (today) | none: agents, terminals, browsers, editors, other nested proofs and indexing share the uid | exact | genuine, but the run is the developer account's | cheapest; the retained failures are reproduced by design | not a qualification route |
| B | Dedicated same-host macOS proof account, automation (SSH) session only, same host, same Stash volume, per-account scratch parent | strong: the account's few launchd user agents plus the run's own processes; the developer account becomes foreign-uid and non-blocking | exact volume, new route | genuine, with the account's own identity | one local user, one toolchain, one checkout, one scratch parent; small residual | **recommended** |
| C | Dedicated same-host account with an interactive GUI login | as B, plus the Aqua user agents and Finder, QuickLook and Spotlight activity in that uid | exact volume, new route | genuine | as B, with a materially larger and less predictable same-uid set | fallback if B cannot be automated |
| D | Disposable dedicated macOS host, Stash bound as the same APFS device | total for that host | exact only if the same volume is exclusively attached | the new host needs its own genuine admission | new host identity, device binding and exclusive-attach rules; APFS cannot be safely attached to two machines | only if B is shown insufficient, and only with a host/device binding decision |
| E | Disposable macOS VM with a share or a VM-internal volume | total for the guest | **not** the pinned volume: a share is not an APFS volume with a UUID, and a VM-internal volume mints a new UUID | no genuine host admission authority exists in the guest | replaced premises, plus Apple virtualisation and licensing | refused while the UUID premise is pinned; a different claim |

A *dedicated window on the existing account* (developer logs out, runs nothing)
is not a separate option: it is A with a promise, leaves background agents,
login items and residual helpers in the same uid, needs exclusive use of a host
that other tasks share, and offers no mechanism in place of the promise.

## Recommended candidate: B

A dedicated, non-admin macOS account on the same host, used only for Barge proof
occurrences, reached over SSH (or a non-Aqua automation session), with its own
toolchain, Oxide installation, checkout, Nucleus/Effigy identity and its own
owner-only scratch parent on the already-selected Stash volume.

Assumptions, stated so the reviewer can reject them:

1. The operator owns the account, its credentials and its first installations.
   No agent holds an admin password or the account's password.
2. Nothing interactive runs in that account during an occurrence; the launcher
   is the only session, and a second session is a refusal condition, not a
   hope.
3. The per-account scratch parent is a new directory on the selected volume and
   the existing `/Volumes/Stash/scratch/barge` parent, and every retained root
   beneath it, is not read, statted, re-owned or re-moded.
4. The proof's scratch-route constant is extended to name that per-account
   parent (difference D1 below) by its own reviewed source change before any
   occurrence; until then the shipped constant is correct and unusable from the
   new account.
5. The account's own genuine Effigy heavy admission and Nucleus run identity
   exist (prerequisite P1). If they do not, the occurrence does not run; a
   developer ticket is never copied and the guard's environment is never
   hand-set.
6. Occurrences stay within the fixed bounds above; `oxide`'s compiler output
   keeps its ordinary pool placement.

Why B and not A: uid separation converts the developer account's processes into
foreign-uid rows that Q-007 classifies and reports without inspecting, so a
browser, an editor, a second agent or a nested build can no longer make the
scan unknown. Why B and not C: an Aqua session adds exactly the per-user daemons
that Q-008 had to rule on, and each is a blind spot only while its code-signing
status verifies a platform binary with a stable start identity; a
non-platform helper in that set refuses. B keeps that set as small as macOS
allows without changing the host. Why not D or E now: both replace a premise
the proofs verify (host identity, admission authority, or the volume UUID) and
neither is needed to remove the dependency the retained facts show.

B is not isolation and does not promise a pass. The residual same-uid producers
are:

- the account's own launchd user agents and any login item, which are readable
  in the ordinary case and a counted blind spot only under Q-008's conditions;
  a same-uid non-platform process with a hidden descriptor still refuses;
- any process the launcher, an operator or an automated agent starts in that
  account during the window, including a second proof or a shell;
- the guarded child's own descendants while staging runs, which are this
  account's and are reaped before the cases by the existing custody rules;
- a process created after the scan began, which the product does not observe by
  declared limitation, and a process appearing mid-scan, which can only be seen
  partially;
- unrelated readable descriptors anywhere in the account, which are harmless:
  the scan matches the fixture root's vnode, and only unreadability, movement or
  a bound refuses.

A bounded, read-only census of the proof account's own processes (pid, command,
start identity only) taken at readiness is the honest evidence that B is as
quiet as claimed. A quiet preflight is a precondition observation for that
occurrence; it is never proof that a future writer is absent.

## Topology and identity boundaries

- **Provisioner.** Tom, on the host's admin account: creates the proof account,
  decides the scratch parent's path and ownership, authorizes the first
  toolchain and Oxide installations, and authorizes each occurrence. The
  provisioner is the only holder of the account's credentials.
- **Launcher.** The proof account's own automation session (or its own genuine
  Nucleus worker) starts the Effigy selector. It is the identity whose uid the
  product's scan reads.
- **Custodian and guard.** The qualified supervisor's own custodian, monitor and
  guarded child, unchanged. The proof driver never launches, substitutes or
  extends them.
- **Ownership.** The proof account owns its scratch parent, guard roots, staged
  artifact, image, checkout, cargo and rustup homes and credentials. The
  developer account keeps the existing scratch tree and the retained images; a
  private 0700 parent makes them unreadable to the proof account, and they are
  not inspected by anything in this route. Barge's product behaviour, the
  scanner, the formats and the supervisor are unchanged.
- **Admission.** Effigy `heavy` admission and the Nucleus run identity are the
  proof account's own, obtained through the documented interfaces. Until the
  Queue admission-hook adapter (Queue #381) lands, the interim allowance's
  disclosure is carried unchanged and in full: "Effigy heavy admission
  unavailable (f6b3b50 retired host-run client; Queue #381 pending); not
  host-admitted", beside the fresh scheduler headroom receipt. Tickets are
  never transferred between accounts, guard variables
  (`OXIDE_TASK_SCRATCH_ROOT`, `OXIDE_PROOF_EVIDENCE`) are never fabricated, and
  no production or retained root is reused as a fixture parent.

## Volume and filesystem reconciliation

Same volume, same mount, same UUID: the per-account parent is a new owner-only
directory on `8A660440-C919-45C6-A3F1-3365B9BD2006` at `/Volumes/Stash`. The
guard's own layout is unchanged — one `oxide-proof.*` root directly under
`--root-parent`, then `scratch/` and `evidence/` as siblings inside it, the
owned root owner-only, the scratch and evidence directories private to this
account, and the run's 64 MiB sparse APFS image created inside that root. The
route document the guarded half re-checks must name the same parent and the same
volume UUID, and an aliased, symlinked, foreign or off-parent root still
refuses.

Two differences are named rather than substituted:

- **D1, route parent.** The proofs pin `--root-parent /Volumes/Stash/scratch/barge`
  and refuse a parent that is not the canonical owner-only scratch of *this*
  account. Two accounts cannot both own that directory, and widening it (a
  shared group, a 2770 mode) would let either account create, rename or remove
  the other's roots beside three open barge#061 images and the other retained
  roots. The proposal is a sibling parent on the same volume, owned 0700 by the
  proof account, named by a reviewed change to the proofs' route constant and
  route-document check. That is a route change, not a premise relaxation: the
  volume, mount and UUID premises are unchanged, and no existing root is touched.
- **D2, a second volume or a VM.** A new APFS volume, a new device binding or a
  VM share replaces the pinned UUID or the host identity. Each is a premise
  difference for Tom to decide separately, with its own acceptance claim; none
  is accepted here, and none may be written into a proof as an equivalent.

## Failure-time attribution (proposed evidence contract only)

The design proposes, for a later reviewed source change, that a decided custody
refusal or a failed case publishes — at the seams that already exist — what a
reader needs to judge the environment without a new campaign:

- for every non-clean classification: pid, kernel start identity, effective uid
  and role (foreign uid, same-uid readable, same-uid hidden descriptor,
  unverifiable platform-binary status, dead handle, exited);
- for each vnode descriptor it read: per attempt 1..3, both reads' answers and
  errno, whether the pair agreed, and the elapsed monotonic time inside the
  scan's existing 10 s bound;
- the existing per-row `custody` verdict and reason, identity, reference and
  absence verdicts, and the guard's own receipt facts (session and closure), as
  separate records that are never merged into one verdict.

The seams are the product's `Safe`/`describe.custody` evidence, the proof's
deciding plan-row and command-fault records and the supervisor's receipt. No
copied scanner, second log pipeline, daemon, supervisor change or new liveness
policy follows, and no product decision changes: the record explains a refusal
that already happened. Missing past facts stay unknown; no host, PID or
historical runtime inspection reconstructs why an earlier census did not
complete.

## Prerequisites and refusals

- **P1 — the account's own admission identity.** Does the project's Queue and
  Effigy admission issue a genuine run identity to a second local account? If
  not, the native chain cannot start there, and the answer is a refusal to
  transfer tickets, not a workaround.
- **P2 — the account's toolchain and Oxide.** `1.97.1` installed in that
  account's own rustup home, and Oxide installed for that account; Oxide's own
  installation authority applies and Barge changes nothing in Oxide.
- **P3 — the exact-head checkout** is a fresh clone in that account at the
  pushed head under proof, with no link, mount or alias to the developer's
  checkouts.
- **P4 — the per-account scratch parent** exists, is owned 0700 by that account,
  and D1's reviewed route change is merged before an occurrence uses it.
- **Refusals.** No occurrence runs while any premise above is unmet, while a
  second session or an unexplained same-uid process is present, while the
  scratch parent or its volume facts differ, or while the supervisor's bytes,
  toolchain or checkout head differ from the pinned values. A custody refusal
  inside the run stays `infrastructure`, unqualified, with its resources
  retained, exactly as the shipped preflight decides.

## Finite route

Each step is separately authorized; none is implied by the previous one.

1. **Preparation review.** This design goes to one retained independent
   exact-head reviewer, who judges feasibility, premise preservation and the
   authority boundaries. No provisioning follows from approval alone.
2. **Preparatory decisions.** Tom decides P1's answer, the scratch parent's
   path and ownership (D1), and the approval of each provisioning action.
3. **Provisioning and readiness (separately authorized).** The operator creates
   the account, its session policy, toolchain, Oxide, checkout, parent and
   identity, then records readiness: uid, toolchain, checkout head, parent's
   owner/mode/volume UUID, supervisor bytes, admission disclosure and the
   bounded own-account process census. No image, fixture or native is created.
4. **Exact-head uncounted diagnostic (separately activated).** One occurrence,
   keep-going, always `qualified:false`, with its own image closure.
5. **Counted author occurrence (separately activated).** First-fault stop, its
   own image closure, its own completed joined deciding/final certain-empty
   group closure.
6. **Distinct retained reviewer occurrence (separately activated).** Same head,
   independent, its own image closure and its own group closure.
7. **Final independent exact-head correlation** of head identity, both
   receipts, every case row and both closures, before merge and publication and
   before Oxide#304 is re-pinned to any new contract.

No native allowance is spent, extended or refunded here. A missing receipt, an
UNCLOSED or retained image, a skipped row and an unexecuted case are each
missing proof; barge#061's native and merge holds stay until the chain above
completes at one exact head.

## What this design does not establish

It does not provision anything, does not touch or explain the retained images
and does not establish availability, a native result, an apparatus defect, a
host-transient cause, certification, adoption or performance. It does not make
the product's scan succeed, relax its refusal contract or exclude any process.
It supplies no Oxide#304 credit: that consumer's own 600 s / 512 MiB / 128 MiB
gates, activations and reviewer judgments remain its own.
