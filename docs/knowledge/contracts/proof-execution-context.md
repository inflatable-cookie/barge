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
foreign. A separate guest can remove host same-uid processes from the guest product scan;
it does not make the guard census complete or ensure that guest processes are quiet. No claim is made that a quieter
context would have prevented the historical stall.

## Selected context and authority

Tom reaffirmed on 2026-11-12 that no new accounts are wanted on this host and
that the existing Parallels macOS VM is the intended environment (decision
`49861fc2` v3). This supersedes barge#062's recommended second host account
and its P1 about second-account admission. Its same-uid analysis and historical
results above remain valid. A guest is a different process and kernel context,
not a promise that custody will pass. No new host account, VM, daemon or Nucleus
participant is assumed. The earlier preparation approval and this correction
activate no VM start, setup, source change or proof.

Owner-observed host metadata describes an existing stopped ARM64 macOS VM with
its backing on Stash, host shared folders/profile disabled and GuestTools
reported outdated. These are point-in-time observations, not a current device
identity or evidence of guest OS/build, tools, credentials, storage, admission,
resource availability or closure. Do not inspect its backing to fill those gaps.

## Transport and identity

The retained host author or distinct reviewer remains the genuine Nucleus
participant: their own run-bound report records the role, exact source head,
activation and returned evidence. The guest executes the proof; it does not
need a Nucleus participant merely because the host commands it. The installed Nucleus skill (`SKILL.md`, “Calling thread identity”) describes
owner-only participant tickets and authenticated run-bound callbacks; those are
not guest credentials. An owner-only host service/socket and ticket cannot simply be used
across a different UID or machine boundary. Never copy a ticket, impersonate a
participant or fabricate guard environment variables.

The installed `prlctl exec --help` documents executing a command with arguments
in a specified VM, including current-user or specified-user authentication. It
is a candidate command transport, not proof that authentication, GuestTools,
exit handling or output capture works in this guest. Its help does not establish
an evidence-file transfer interface. The minimum route is a host participant
using an authorized, verified guest command channel, delivering the exact
source and retrieving complete guest-produced receipts and manifests through a
verified byte-preserving channel. Bind the run, role, source/tree/lock/toolchain
and staged binary hashes, guest route identity, guard and case receipts, exit
status and evidence digests to the host report; verify bytes after transfer and
retain originals and failures. Host stdout alone is insufficient if receipts or
closure evidence are missing or truncated. The concrete unresolved capability is
an authenticated, byte-preserving return of complete evidence from this existing
guest to the host participant, with checked status and provenance; choose and
verify an available documented channel during separately authorized readiness.
No SSH availability, shared folder, credentials or new platform framework is
assumed. If this cannot be demonstrated, the occurrence refuses.

Host participation is not guest resource admission. Before any occurrence,
separately establish genuine applicable admission and capacity for both the
host-controlled work and guest execution, with the actual admission mechanism
and its limitations recorded; do not transfer a host allowance as guest proof.
The existing interim disclosure, if applicable, remains explicit: “Effigy heavy
admission unavailable (f6b3b50 retired host-run client; Queue #381 pending); not
host-admitted”, alongside a fresh scheduler headroom receipt. Use it only after
the current Barge and VM applicability and every published condition are
established, including a continuous 20 GiB reserve watchdog, raw qualified
receipts, unchanged unknown and closure handling, and its stated end condition.
Neither a host Nucleus report nor command transport supplies guest admission or
guard custody. Missing guest evidence is not a finding that a mechanism is
unsupported; it stops the occurrence until the particular fact is established.

## Guest storage and proof route

Stash holding a VM bundle does not make the guest's disk the host Stash APFS
volume. A shared directory is not an APFS volume and cannot stand in for guest
mount, image or kernel semantics. The current [release entry](../../../scripts/native/release-proof.py)
pins the host mount, canonical owner-only parent and UUID
`8A660440-C919-45C6-A3F1-3365B9BD2006`, checks them before the guard, and
rechecks the carried route in its guarded child. The
[listing entry](../../../scripts/native/descriptor-listing-proof.py) uses the
same route and guard. Those exact bindings and source identities remain true for
old occurrences; a guest UUID must never be presented as equal to them.

A later reviewed source and premise amendment must bind a freshly identified
guest-owned APFS volume, mount and canonical owner-only scratch parent, with no
home, volume-root or symlink route. It must verify UUID, mount, ownership and
containment before effects and again in the guarded child, and bind those facts
and the exact amended source to each receipt. Under that parent the unchanged
guard creates its private run root, scratch and evidence; release creates its
own bounded APFS image there and proves ordinary detach, reap and whole-backing
attachment absence. Listing has no image. Missing or uncertain identity or
closure refuses and retains the owned resources. Guest APFS and image results
can exercise guest kernel, mount and image behavior, not establish physical
external-drive identity, host removable-device behavior, live-drive adoption or
host process custody. Physical removal coverage needs its own scoped route.

## Guard and unchanged limits

Tom selected the official pinned Rust 1.97.1 guest setup in decision
`3f774e4b-546e-48f1-8312-46991f450cfb` v3. The source amendment may use
plain Cargo only in the explicitly bound VM guest; ordinary host builds remain
on Oxide. Guest setup and each proof occurrence remain separately activated.
An owner-selected, digest-bound invocation record is scoped instruction data,
not guest authentication or proof admission. The retained host participant
binds its role, run, activation and fresh registered VM observation in its
genuine report. The guest rechecks only its own account, volume, source,
toolchain, guard and containment; it cannot authenticate the host's VM UUID or
Nucleus role. A nonce correlates evidence but establishes neither admission nor
closure. This is a cooperative trusted-host workflow, not a defense against a
malicious host, guest or root user. Complete transport, applicable admission,
private Data scratch and actual runtime fit still require separate evidence.

The guest must actually run the same hash-pinned Oxide qualified supervisor
`fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b`
through the proof's outer entry, with guest Python, verified bytes and a guest
route authorized by the reviewed amendment. The outer entry passes the guard
its original wall and scratch limits and the inner entry validates the guard's
own scratch, evidence and deadline; the current host route stages fresh inside
that guard through ordinary Oxide, with locked offline toolchain `1.97.1`.
The guest Cargo route must retain those stage and source-binding requirements.
Verify actual guest
availability and compatibility rather than assuming the host installation is
available. The supervisor monitors its guest-owned resources and performs its
own guest session/process census and joined closure. A host `prlctl` process
exiting or being reaped, a lost transport, or stopping the guest proves neither
that guest producers closed nor that an image detached. Missing receipts and
uncertain closure are failures with resources retained.

Release retains 300 s / 512 MiB, one 64 MiB image and the 30 s work stop;
listing retains 600 s / 512 MiB, no image and the 30 s stop. The original
nine premise dispositions, case tables, source-bound staging, scanner limits and
unknown policy remain in force unless a separately reviewed amendment explicitly
changes a premise and its claim. There is no second supervisor, scanner
exception, broadened ceiling or automatic retry. The supervisor's session-scoped
census is separate from the product's same-uid whole-guest scan; neither
establishes the other's closure.

For the 061 guest apparatus amendment the nine premise dispositions are:

1. **Private HOME and rustup:** fixture children use private HOME and CARGO_HOME;
   they do not build or install a toolchain. The guest build binds the official
   toolchain, committed lock and complete offline inputs; the ordinary host
   route retains its existing rustup handling.
2. **Account home:** fixture children use the synthetic account home inside
   the guard's owned scratch.
3. **Environment:** inherited hooks and compiler flags are scrubbed; source,
   tool and route identities are independently rechecked inside the guard.
4. **Adoption:** the proof creates synthetic roots explicitly; no fixture builds
   or live adoption are implied.
5. **Cargo external argv:** these cases invoke no fake Cargo external
   subcommand. The separately staged guest build uses pinned locked, offline
   Cargo; ordinary host builds continue through Oxide.
6. **Owned image:** the one fresh image needs ordinary detach, reap and
   whole-backing absence; uncertain custody stays retained.
7. **Compiler flags:** inherited Rust flags and wrappers cannot alter the build;
   tool and input identities remain part of the bound source evidence.
8. **Temporary storage:** guest build output and staged binary stay inside the
   existing guard's monitored 512 MiB root and 300 s occurrence.
9. **macOS mktemp:** Python's `mkdtemp` receives an explicit owned parent;
   the Data leaf and stage directories are exclusive and verified.

These are requirements of the amendment, not observations of a guest run.
Current admission, byte-preserving transfer, the guard's actual guest closure
and each runtime occurrence remain separately evidenced and activated.

## Readiness and later authorizations

1. **Source preparation now:** the scoped 061 guest Cargo apparatus amendment
   and effect-free covering checks are authorized. No guest survey or backing
   inspection is authorized. Source readiness requires independent exact-head review.
2. **Later, separately authorize VM start and guest inspection:** establish the
   existing guest identity, OS/build, command and evidence return channels,
   admission and resource headroom, APFS mount/UUID and required tool availability.
   A failed or missing fact remains unresolved; do not invent a credential or
   change GuestTools.
3. **Later, separately authorize any necessary setup/install:**
   use the existing guest identity; bind and review the fresh guest scratch route,
   exact source and premise differences, toolchain, Oxide and supervisor bytes.
   No setup or installation follows from this document.
4. **Later, separately activate each exact-head occurrence:** adequate uncounted
   diagnostic first, counted author and distinct retained reviewer after that,
   each with its own prospectively authorized scope and image closure where
   applicable, plus separate qualified completed joined deciding and final
   certain-empty group closure. Final exact-head judgment is still required.
   A transport or guest failure never converts missing evidence into a pass.

The three open barge#061 images, seven oxide#304 images and all other unclosed
obligations remain untouched. Both failed author counts remain failed and spent;
the latest R6 process cause and partial effect extent remain unknown. No old
root, backing, PID or device path is inspected, reused, repaired or cleaned.
All 061/304 runtime, provisioning and merge gates remain held; 304 still
requires actual qualified 061 publication and its own integrated gates.
