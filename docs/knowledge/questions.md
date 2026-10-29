# Questions

Questions that block or shape work. Reference them by ID from the plan and from
briefs. An answered question keeps only its pointer to where the answer lives.

## Q-001 — What does each placement kind do when its volume is absent?

Status: answered 2026-10-26
Answer: per kind: build output uses a visible local fallback; caches and scratch fail clearly; kept data is unavailable (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-002 — Which volume owns which data when several are registered?

Status: answered 2026-10-26
Answer: one home per placement chosen by durability: expendable data on Builds, kept data on Vault; moves are explicit (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-003 — How does Barge change another tool's configuration?

Status: answered 2026-10-26
Answer: links first; a tool's own setting is written only during an explicit adopt, reported and undoable (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-004 — How does Oxide's existing state migrate?

Status: answered 2026-10-26
Answer: Barge reads Oxide's existing formats in place; Oxide switches one part at a time (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-005 — Which platforms are in scope?

Status: answered 2026-10-26
Answer: macOS on APFS only for now, with an OS layer throughout so Linux can follow later (Tom); see [architecture](architecture.md#platforms).

## Q-006 — Does kept data have to sit outside `root_dir`, or only outside its managed folders?

Status: answered 2026-10-26
Answer: outside Barge's own folders and every declared area, not outside `root_dir` (design freeze d3e170db, O-004, matching Tom's ruling c80e2034). The Vault needs no layout change; see the [stage 1 design](contracts/stage1-design.md#open-design-issues).

## Q-007 — Can a same-uid descriptor scan read every process it must decide?

Status: answered 2026-10-26
Answer: it does not have to. Tom's ruling bcaa83d8 (carried by barge#011 v2)
decides that each candidate is classified from the kernel's own process table
(`sysctl`/`KERN_PROC_ALL`: effective uid and start identity, no privilege) before
any descriptor is inspected; a process of another effective uid is the named
blind spot, reported in the `Safe` evidence and in `describe.custody`, and does
not block. An unreadable effective uid and a same-uid descriptor that cannot be
read stay `Unknown`. The measured reason it mattered: `PROC_PIDTBSDINFO` answers
`EPERM` for another uid's processes (357 of 1328 on macOS 26.6.2, 25G83), so
identifying processes that way made every whole-system scan `Unknown`. See the
[stage 1 design](contracts/stage1-design.md#writer-custody-and-safe-deletion).

## Q-008 — What about this account's own processes whose descriptors cannot be read?

Status: answered 2026-10-26
Answer: a vnode descriptor whose path and vnode facts the platform both hide is
an OS blind spot for a process its own code-signing status verifies as an Apple
platform binary (`csops`/`CS_OPS_STATUS` carries `CS_PLATFORM_BINARY`, with the
kernel start identity unchanged around the read). It is counted and reported in
the `Safe` evidence and in `describe.custody`, and it does not block; every other
hidden descriptor, a failed status read and an identity that moves around it stay
`Unknown` (Tom, decision e528a712, implemented by barge#013); see the [stage 1
design](contracts/stage1-design.md#writer-custody-and-safe-deletion).
Measured before the decision (barge#011 v2, macOS 26.6.2):
`PROC_PIDFDVNODEPATHINFO` answered `EPERM` for 8 of 1004 processes of this
account — the sandboxed and TCC-protected system daemons `progressd`, `routined`,
`ScreenTimeAgent`, `dmd`, `lsd`, `nsurlsessiond`, `UserEventAgent` and
`com.apple.quicklook.ThumbnailsAgent` — while listing their descriptors was
allowed. That made a whole-system scan `Unknown` under the rule that every
unreadable descriptor refuses, and so a full-scan `Safe` verdict unreachable.

## Q-009 — Should Barge manage VM mounts, such as Colima profiles?

Open (raised by Effigy, Tom 2026-10-27). Container fixtures run in a Colima VM
whose profile mounts only `$HOME`, so files that `nerdctl compose` must open
cannot live in Barge's scratch on the Builds drive. Proposed direction, not
designed: a VM mount is another tool-configuration placement under Q-003. Barge
would write a profile's `mounts:` entry only on explicit adopt, mount only a
declared folder (for example `scratch/`) and never the whole volume, report the
change exactly and undo it. It would also need a rule for an absent volume at VM
start. This is not in stage 1. Until it is designed, mount changes stay manual
operator changes to the Colima profile.

## Q-010 — What about a vnode descriptor that is dead (revoked or reclaimed)?

Status: answered 2026-10-28
Answer: both of a vnode descriptor's per-fd reads are evaluated, and only a
readable path with readable vnode facts, `ENOENT` from both and `EPERM` from both
are an answer. A mixed pair is re-read as a pair up to three times about 10 ms
apart inside the scan's own time bound, and the first agreeing pair is used; a
process that has exited at a re-read holds nothing; three attempts that still
disagree refuse as `Unknown`, including a readable path whose vnode facts answer
`ENOENT` and a path read that answers `ENOENT` beside a closed vnode-facts read
(Oxide Supervisor 1a5b31a0, barge#008). A descriptor the platform reports closed
is never an answer at all: `ESRCH` and `EBADF` are other errnos. A closed
retry pair is followed by a bounded re-listing: an omitted fd is gone; a listed
fd is paired again and a still-closed pair refuses. Outside that retry, two
closed answers are `Unknown`, as is one closed answer beside any other answer —
whichever read saw the close — because no read's answer is carried by the other's
and the fd may already have been closed and reused. When both reads answer `ENOENT` after a
successful `PROC_PIDLISTFDS`, the scan re-observes that descriptor under
barge#012's bounded rule, re-reading the pid's kernel identity and rechecking both
deadlines after the observation and the final identity read, and reports a **dead
handle** — pid, start identity, descriptor and command — in the `Safe` evidence
as `dead_handles` and in `describe.custody`, where it does not block: a revoked or
reclaimed vnode cannot do I/O. `ENOENT` on the listing itself, a changed or
unreadable identity, a bound reached and a second observation that fails — which
is never discarded, not even when the pid is then reported gone — stay `Unknown`,
and no path or name is listed for a dead handle (Tom, decision e33115d3, implemented by barge#023); see
the [stage 1
design](contracts/stage1-design.md#writer-custody-and-safe-deletion).
Measured before the decision (barge#008, macOS 26.6.2): a live same-uid
terminal pty-host held descriptor 33, whose `PROC_PIDFDVNODEPATHINFO` and
`PROC_PIDFDVNODEINFO` both answered `ENOENT` persistently and from every context
while its descriptor listing succeeded, which made the product-custody census of
a full-scan proof unreachable.

## Q-011 — What makes an interrupted release unknown?

Status: open — needs the design delegate (Oxide Supervisor); recorded by barge#024
(slice T3) at the exact-head review's request (comment 6498979111).

A consumer's release is one order of two steps: the activity fact is written, then
the hold file is removed (or the reverse). Whichever order the frozen design
fixes, an interruption between the two steps leaves a pair that the frozen
`barge.activity/1` fields cannot distinguish from a completed release:

- fact first (implemented today): a published release plus a hold file. A live or
  undecidable hold keeps the root; once the holder is positively gone the hold is
  **stale**, stage 1 defines a stale hold as not blocking idleness, and the
  published fact decides — so an interrupted release reads as idle, and the LRU
  key is that release's time.
- removal first (the activity section's sentence): an absent hold plus the
  previous record. The root reads idle on an **older** release time (or, with no
  earlier release, is kept).

Both were raised in review, in opposite directions (rounds 2 and 3 of barge#024),
and both violate "unknown is never idle" in the window. The record is one atomic
publish, so no ordering of the two steps closes it.

Proposed answer, modelled on the pattern stage 1 already uses for this exact crash
window (a durable phase published before an effect, a completion published after
it, and the in-between state `Unknown` rather than complete — "the crash window
between the successful final unlink and the durable `removed` phase is therefore
pending, not complete"): give the activity record a `pending_release_ms` field,
written before the hold removal and cleared by the completing write, with the
idleness matrix refusing while it is set. Concretely, one release would be: read;
write `pending_release_ms: now`, `generation + 1`; remove the hold; write
`last_release_ms: now`, `releases + 1`, `generation + 1`, `pending_release_ms:
null`. An interruption anywhere leaves `pending_release_ms` set, so the root is
never idle on an unsettled release; a failed hold removal restores the previous
bytes (clearing the pending flag) and reports `io`; a failed restore leaves the
flag set, which is unknown rather than a release. The repair is the same explicit
`barge area idle-baseline` (or re-running the release, which settles it).
Alternatives, each needing the same ruling: a separate pending record under
`state/activity/` (a second authoritative document); a stage 1 intent phase for
releases (a new intent kind in a closed set); or accepting the window and saying
so, which the current text does.

The field is a format decision, not an implementation one: `barge.activity/1` is
frozen with an exact field set, and the design freezes are the Oxide Supervisor's.
Until it is answered, the implemented surface records the window and the slice's
regressions pin today's outcome, so the answer is a visible change.
