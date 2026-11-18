# Questions

Questions that block or shape work. Reference them by ID from the plan and from
briefs. An answered question keeps only its pointer to where the answer lives.

## Q-001 — What does each placement kind do when its volume is absent?

Status: answered 2026-10-26
Answer: per kind: build output uses a visible local fallback; caches and scratch fail clearly; kept data is unavailable (Tom); see [architecture](architecture.md#absent-volume).

## Q-002 — Which volume owns which data when several are registered?

Status: revised 2026-11-18
Answer: each resource class is placed on one registered volume, and different classes can live on different volumes; durability belongs to the class, not the volume (Tom, 2026-11-18, replacing the 2026-10-26 Builds/Vault split); see [resources](resources.md#resource-classes).

## Q-003 — How does Barge change another tool's configuration?

Status: answered 2026-10-26
Answer: links first; a tool's own setting is written only during an explicit adopt, reported and undoable (Tom); see [resources](resources.md#bindings) and its [adapters](resources.md#adapters).

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

Status: answered 2026-11-18
Answer: in both places, with one authority. The account layout is authoritative and covers every volume, so Barge knows where classes belong while a drive is absent. Each volume carries a record of the classes it holds, for import and drift checks (Tom, 2026-11-18); see [resources](resources.md#layout).

## Q-015 — How do agents get their scratch and worktree locations?

Status: answered 2026-11-18
Answer: without coupling. Each tool keeps its own location setting, which Barge sets and checks. Processes inherit locations from the tool that starts them through the standard `TMPDIR`. Interactive agents read a generated `AGENTS.md` block. Nucleus and Effigy change nothing and never depend on Barge (Tom approved, 2026-11-18); see [resources](resources.md#reaching-tools-and-agents-without-coupling).
