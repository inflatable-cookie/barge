# Coordinator fixture proof preparation

The [single-authority coordinator](stage4-design.md#implementation-status-the-journalintegrity-store-barge058)
is being prepared for an independently reviewed fixture proof. The
[apparatus](../../../scripts/native/operation-coordinator-proof.py) declares
the held native modes; this source declaration does not activate a run.
The diagnostic, counted author and distinct reviewer occurrences each require
separate prospective owner activation at the exact reviewed head.

`effigy coordinator:native:test` drives the actual outer and inner entries,
environment, stage receiver, case receiver and report/closure composition
with fake process, image, Git and clock-facing boundaries. Its writer control
exercises the actual readiness receiver with a fake direct child and a private
file. `effigy coordinator:native:plan` prints the held case map and bounds.
Neither attaches, builds a binary or proves a kernel outcome.

## Proposed fixed case map

Each case is a separate driver invocation with fresh private state, a fresh
synthetic account home and a distinct root on the single owned image. The
driver uses the actual checked coordinator, store and default delete path.
The caller's own atomic settlement entry is domain fixture state, not Barge
progress or a second deletion journal.

| Case | Required observation |
| --- | --- |
| `positive` | Actual deletion is recorded and the linked handler settles. |
| `owed` | Missing, unsupported and failing handlers leave finalization owed. |
| `lost-ack` | A persisted caller settlement and lost reply replay idempotently. |
| `concurrent` | Two dispatches consult under the caller guard and settle once. |
| `busy` | A held journal lock makes the fail-fast consult refuse. |
| `cancel` | The original root remains and a no-effect cancellation is retained. |
| `withdraw` | A withdrawn original grant refuses before capture. |
| `mismatch` | A different original request refuses before an answer or effect. |
| `intent-capture` | A labelled post-rename publication fault leaves caller intent; generic replay refuses and fresh reaffirmation can continue. |
| `renamed-replay` | A labelled stop after durable capture is replayed by Barge. |
| `removed` | Barge's actual unlink precedes the durable `removed` phase, which replay settles. |
| `unlink-window` | A labelled post-unlink publication failure stays pending; absence is not completion. |
| `held-writer` | A fresh direct child holds an actual writable descriptor and the real scan identifies that child as unsafe. |
| `unknown-writer` | A labelled proof-only failure at that exact child's rechecked descriptor is consumed, and production custody reports unknown with no effect. |

Planner ruling `3fa7d74d0606b724503807c666093730ae03b2b33cdc941d4ec714942194cc76`
permits this narrow proof-only descriptor-path EIO injection. The driver binds
the fresh child's actual PID, start, effective UID, descriptor, device and
inode, and the OS seam rechecks those facts against the real reader before it
consumes the fault. All other reads, process enumeration, limits and retry
policy remain real. The receipt labels the injected query and requires that
exact target to consume it; this does not claim a natural kernel denial. The
guard disarms on drop, including error paths. An earlier unrelated refusal or
an unreaped child fails the case. An uncontrolled host stall, a synthetic
observer or a targeted listing result cannot fill that gap.

## Nine premises and closure

1. Each case receives a private `HOME`, synthetic account home and explicit
   Barge state directory within the guard's owned scratch. The build uses the
   existing Oxide-managed build route and pinned toolchain.
2. The `proof-synthetic-account` feature validates the private Oxide account
   binding before fixture writes. The proof makes no production account lookup
   or live-registry claim and does not inspect real account state.
3. Case temporary variables all name the case directory. The inherited build
   and case environment is scrubbed by the existing release stage helper.
4. The fixture explicitly registers only its fresh image UUID and declares
   only its private area and grant. There is no installation or adoption.
5. One 128 MiB APFS image uses the accepted image helper: publish its custody
   indication before backing creation and attach, verify owner, backing,
   mount, device and UUID, and use only ordinary detach. An uncertain attach
   or detach retains the backing and refuses the occurrence.
6. The proof features are `proof-journal` and `proof-synthetic-account`.
   Publication, phase-stop and the single descriptor-read fault are explicitly
   named; the writer cases use a real descriptor held by a fresh direct child,
   not an observer stub.
7. The runner, case directories, image backing, stage copy and report live in
   the supervisor-owned scratch or evidence directory. Compiler output uses
   the existing Oxide-managed route.
8. Each temporary directory is created with an explicit verified parent.
   No pre-existing fixture or binary is reused.
9. The unchanged qualified supervisor is verified by its pinned digest
   before launch. Its independent receipt must name this command and the fixed
   600 s / 512 MiB bounds, and show a joined census with both deciding and
   final observations certain and empty. Holder reap is not group closure.

Each occurrence builds its own proof-feature driver inside the guard. Its
manifest binds clean source head and tree, lockfile, toolchain, driver and
helper sources, feature set and staged binary hash; a before/after source or
copy mismatch refuses. One absolute deadline governs staging, cases and
closure. No new work starts in the final 30 seconds; image operations and child
timeouts are capped by the remaining window. These are proposed bounds, not a
measured fit: the planning allowance is 0–300 s for the fresh build,
300–500 s for image setup and the finite cases, 500–570 s for any remaining
case, ordinary detach and report, and 570–600 s for supervisor group closure.
The single absolute deadline, not these estimates, decides whether any work
may start. Each case has at most 60 s within the remaining work window. The
synthetic-account feature does not qualify production
account lookup, a live drive, or an Oxide integration.
