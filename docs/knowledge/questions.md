# Questions

Questions that block or shape work. Reference them by ID from the plan and from
briefs. An answered question keeps only its pointer to where the answer lives.

## Q-001 — What does each placement kind do when its volume is absent?

Status: answered 2026-10-26
Answer: per kind: build output uses a visible local fallback; caches and scratch fail clearly; kept data is unavailable (Tom); see [architecture](architecture.md#absent-volume).

## Q-002 — Which volume owns which data when several are registered?

Status: answered 2026-10-26; revision proposed 2026-11-18
Answer: one home per placement chosen by durability: expendable data on Builds, kept data on Vault; moves are explicit (Tom); see [architecture](architecture.md#concepts).
Proposed revision: there is one external drive, Stash, holding both. Durability belongs to the resource class, not the volume, and kept classes sit outside every expendable area; see [resources](resources.md#resource-classes). Awaiting Tom's sign-off.

## Q-003 — How does Barge change another tool's configuration?

Status: answered 2026-10-26
Answer: links first; a tool's own setting is written only during an explicit adopt, reported and undoable (Tom); see [architecture](architecture.md#later).

## Q-004 — How does Oxide's existing state migrate?

Status: revised 2026-11-17
Answer: nothing moves on disk: consumers declare their areas and adopt existing directories under their current folder names. Barge records ownership without reading consumer formats; Oxide keeps its own records (Tom, 2026-11-17); see [architecture](architecture.md#library-surface).

## Q-005 — Which platforms are in scope?

Status: answered 2026-10-26
Answer: macOS on APFS only for now, with an OS layer throughout so Linux can follow later (Tom); see [architecture](architecture.md#platforms).

## Q-006 — Does kept data have to sit outside `root_dir`, or only outside its managed folders?

Status: answered 2026-10-26
Answer: outside Barge's own folders and every declared area, not outside `root_dir` (design freeze d3e170db, O-004, matching Tom's ruling c80e2034). The Vault needs no layout change; see [architecture](architecture.md#layout-on-a-volume).

## Q-009 — Should Barge manage VM mounts, such as Colima profiles?

Open (raised by Effigy, Tom 2026-10-27). Container fixtures run in a Colima VM
whose profile mounts only `$HOME`, so files that `nerdctl compose` must open
cannot live in Barge's scratch on the Builds drive. Proposed direction, now
sketched as a `setting` binding in [resources](resources.md#adapters): a VM mount is another tool-configuration placement under Q-003. Barge
would write a profile's `mounts:` entry only on explicit adopt, mount only a
declared folder (for example `scratch/`) and never the whole volume, report the
change exactly and undo it. It would also need a rule for an absent volume at VM
start. This is not in the lean core. Until it is designed, mount changes stay
manual operator changes to the Colima profile.

## Q-013 — Who is authoritative for an area declaration, the stored record or the caller's object?

Status: answered 2026-11-01
Answer: the stored declaration. A library caller presents the same declaration
or is refused before any effect, naming the differing `volume`, `folder`,
`absent`, `soft_cap_bytes` or `counts_toward_reserve`; the first declaration,
when no record exists, publishes the caller's validated declaration, and a later
one can only match it. The effective declaration in a root's marker is the
governing record's (Tom's Q-G decision `12729764`); see the
[architecture](architecture.md#concepts).

## Q-014 — Where does the layout manifest live?

Open (Barge planner, 2026-11-18). Proposed: on the volume, at
`<mount>/.barge/layout.json`, so the layout travels with the drive and every
machine agrees on it. Bindings stay per machine in account state. The
alternative keeps the layout in account state, which is simpler but lets two
machines disagree about one drive. See [resources](resources.md#layout-manifest).

## Q-015 — How do agents get their scratch and worktree locations?

Open (Barge planner, 2026-11-18). Today agents follow hand-written `AGENTS.md`
rules. Proposed: `barge init` writes a generated workflow block, and Nucleus
and Effigy inject `barge env` into the agents and runs they start, so the
location reaches the process without the agent reading any docs. Injection
needs agreement from the Nucleus and Effigy owners. See
[resources](resources.md#agent-workflows).
