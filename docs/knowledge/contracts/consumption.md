# Consumption

Rust consumers depend on the `barge` library at an exact 40-hex Git commit, not
a branch or tag. For example, substitute a reviewed pushed-main revision:

```toml
barge = { git = "https://github.com/inflatable-cookie/barge", rev = "<40-hex-commit>" }
```

Commit the consumer's lockfile. Offline builds need the locked Git commit and
its locked dependencies seeded in the consumer's Cargo cache in advance. A
missing candidate is an offline failure, not permission to resolve a moving
ref. Tags and releases are operator-gated.

Non-Rust consumers use the CLI's `barge.<kind>/<n>` JSON documents. The
document schema name and integer version identify the contract; consumers must
check both and handle unknown versions rather than assuming a compatible
shape. [Architecture](../architecture.md#cli) owns the CLI's verbs and exit
codes.

Move plans are `barge.move-plan/2` documents, printed directly by `barge move
--plan` and supplied unchanged to `barge move --apply --plan`. A version 1 plan
is refused: make a fresh one. A `barge.bind-plan/2` document records the
destination's device and inode as well, so `barge bind apply` refuses a
destination that was replaced since it was reviewed; a version 1 plan is refused
the same way. A plan lists the links at [known link locations](../resources.md#known-link-locations)
that no binding records, and applying it needs `--retarget-unrecorded` while it
lists any. Successful JSON apply output is a `barge.move/1` envelope containing
the plan and doctor report, or the doctor error if that subsequent check fails. The move is already committed in either case.

## Success documents

A success document is `{"schema": "...", "data": {...}}` on **stdout**. Every
field below is written as `name: type`, and a type is one of:

- `string`, `int`, `bool`, `float`, `null` — one JSON kind;
- `[T]` — an array whose every element is a `T`;
- `{a: T, b?: T, *: T}` — an object with those fields (`b?` may be absent) or,
  with `*`, any number of keys whose values are a `T`;
- `A|B` — either shape, which is how a variant is written;
- `T?` — a `T` or `null`;
- a shape name from [Shapes](#shapes).

`crates/barge-cli/tests/contract.rs` parses this table, this grammar and the
Shapes table, and holds every command's real document to them, nested objects
included, so the doc and the output cannot drift. It also refuses one field
name carrying two shapes in the `data` level, which is why the name-only fields
below read `_name`, `_key` and `_state`.

| Schema | `data` fields | Commands |
| --- | --- | --- |
| `barge.status/1` | `volumes: [volume-status]` | status |
| `barge.volume-registered/1` | `volume: volume-spec`, `state: volume-state` | volume register |
| `barge.volume-list/1` | `volumes: [volume-status]` | volume list |
| `barge.volume/1` | `volume: volume-spec`, `state: volume-state`, `free_bytes: int?` | volume status |
| `barge.area-declared/1` | `area: area-status` | area declare |
| `barge.areas/1` | `areas: [area-status]` | area list |
| `barge.roots/1` | `area_key: string`, `bytes_included: bool`, `roots: [listed-root]`, `stragglers: [straggler]` | root list |
| `barge.root-action/1` | `area_key: string`, `id: string`, `action: string` | root pin, root unpin |
| `barge.root-evicted/1` | `action: string`, `eviction: eviction` | root evict |
| `barge.prune/1` | `prune: prune-report` | prune |
| `barge.scratch/1` | `area_key: string`, `root: root` | scratch new |
| `barge.sweep/1` | `sweep: sweep-report` | scratch sweep |
| `barge.layout-show/1` | `layout: layout`, `disagreements: [string]` | layout show |
| `barge.layout-updated/1` | `layout: layout` | layout set |
| `barge.path/1` | `path: string`, `fallback: bool` | path |
| `barge.survey/1` | `volume_name: string`, `items: [survey-item]` | survey |
| `barge.binding-list/1` | `bindings: [binding-record]` | bind list |
| `barge.bind-plan/2` | `plan: link-plan` | bind plan |
| `barge.binding/1` | `action: string`, `binding: binding-record` | bind apply, bind adopt, bind undo |
| `barge.move/1` | `report: move-report` | move apply |
| `barge.doctor/1` | `ok: bool`, `volume_reports: [volume-report]`, `binding_reports: [binding-report]`, `misplaced_areas: [misplaced-area]`, `worktree_clone_problems?: [worktree-clone-problem]` | doctor |
| `barge.init/1` | `project: string`, `path: string`, `action: string`, `exclude: string`, `scratch: string`, `volume_name: string`, `volume_state: volume-state`, `warnings: [string]` | init |
| `barge.init-check/1` | `project: string`, `path: string`, `block_state: string`, `exclude: string`, `scratch: string`, `volume_name: string`, `volume_state: volume-state`, `diff: [string]`, `warnings: [string]` | init --check |
| `barge.machine-discover/1` | `discovered: [discovered-machine]` | machine discover |
| `barge.machine/1` | `machine: machine` | machine add |
| `barge.machine-list/1` | `machines: [machine]` | machine list |
| `barge.machine-removed/1` | `removed: machine` | machine remove |
| `barge.machine-wake/1` | `machine_name: string`, `destination: string`, `bytes_sent: int` | machine wake |
| `barge.machine-paired/1` | `machine_name: string`, `client_id: string` | machine pair |
| `barge.machine-unpaired/1` | `machine_name: string` | machine unpair |
| `barge.machine-clients/1` | `machine_name: string`, `clients: [paired-client]` | machine clients |
| `barge.machine-client-revoked/1` | `machine_name: string`, `client_id: string` | machine revoke |
| `barge.machine-status/1` | `machine_name: string`, `mode: string`, `machine_state: machine-state` | machine status |
| `barge.machine-held/1` | `machine_id: string`, `reason: string?` | machine hold |
| `barge.machine-released/1` | `machine_id: string` | machine release |
| `barge.machine-power/1` | `machine_name: string`, `action: string`, `deadline_unix_ms: int` | machine sleep, restart, shutdown |
| `barge.machine-cancel/1` | `machine_name: string`, `cancelled: bool` | machine cancel |
| `barge.worktree-lease/1` | `lease: worktree-lease` | worktree lease, worktree lease get |
| `barge.worktree-return/1` | `action: string`, `lease: worktree-lease` | worktree return |
| `barge.worktree-status/1` | `pools: [worktree-pool]` | worktree status |
| `barge.worktree-prune/1` | `dry_run: bool`, `worktree_prune: [worktree-prune]` | worktree prune |
| `barge.container-status/1` | `leases: [container-lease-status]` | container status |
| `barge.container-return/1` | `id: string` | container return |
| `barge.run-resolution/1` | `resolution: run-resolution` | run resolve |
| `barge.run-status/1` | `machine_leases: [machine-lease-status]`, `machine_holds: [machine-hold]` | run status |

Survey items use `bytes: null` by default. The CLI measures non-system items
only when `barge survey <volume> --bytes` is requested; that walks every
non-system item and can take minutes on a large drive. System items always
have `bytes: null`.

## Run documents

`barge run resolve` reads a bare request object from stdin. The fields are
`platform`, `needs`, `source`, `command`, `env`, `budget`, `fallback` and
`owner`.

- `platform` is `{"exact":{"os":"linux","arch":"aarch64"}}`,
  `{"os":"linux"}` or `"any"`.
- `needs` can include `min_cores`, `min_memory_bytes`, `gpu`, and
  `min_vram_bytes`. Omitted minimums and an omitted or false `gpu` add no
  requirements. An omitted `needs` is equivalent to `{}`.
- `source` has string fields `repo` (URL), `base` (commit), and `tree`
  (expected tree ID). The optional `bundle` and `patch` are initiator-local
  path strings.
- `command` is an array of argument strings. `env` maps names to string
  values and defaults to `{}`. `budget` is an object with integer `secs`
  and `nanos`.
- `fallback` is `"local_allowed"` or `"remote_only"`. `owner` has the
  [container-owner shape](#shapes).

The preview uses `kind: "remote"` with `machine`, `kind: "local_native"`,
`kind: "local_container"` with `profile`, or `kind: "denied"` with `reason`.
Denial is a successful preview. Its reason has a `kind` of
`"no_matching_platform"`, `"unmet_capability"`, `"fallback_forbidden"`,
`"machines_full"` or `"machines_held"`. The library's leased resolution can
also report `"profile_full"` with `profile`, `max`, and `active`.

In run status, `slot` starts at zero. A lease's `ended` is `"owner_ended"`,
`"hold_released"`, or null when an ending has not been established. A hold's
`reason` is null when omitted. Machine identifiers match the strings in
[`run-targets.json`](../runs.md#targets).

Initiators store the [result record](../runs.md#result-record) as a bare object
with `machine`, `platform` (an `os` and `arch` object), `repo`, `base`,
`patch_digest`, `tree`, `toolchain`, `command`, `started_ms`, `finished_ms`,
`waking_ms`, `outcome`, `log` (`digest` and `location`), and `area` (`id` and
`bytes`). `patch_digest`, `toolchain`, `waking_ms`, and `area` are nullable.
The outcome object has a `kind` of `"exited"` with `code`, `"signalled"`
with `signal`, `"infrastructure"` with `stage` and `reason`, `"cancelled"`,
or `"budget_expired"`. The infrastructure stage is `"waking"`, `"syncing"`,
`"toolchain"`, or `"running"`.

## Error documents

A failing command writes `{"schema": "barge.error/1", "error": {"kind": "...",
"message": "..."}}` to **stderr** in `--json` mode, and nothing to stdout. Human mode prints a plain error to stderr. `kind`
is one of `refused`, `not_found`, `usage`, `io`, `unknown_state`,
`unsupported`. [Architecture](../architecture.md#cli) owns the exit codes each
kind maps to. `--json` may appear anywhere before `--` and selects the
document form. `--help` (or `-h`) before `--` prints the deepest matching
command's usage, or the top-level usage when nothing matches, and exits 0
without running anything. `env` emits
shell assignments and refuses `--json`. A move or bind plan is read from a file
or from standard input (`-`). Input that cannot be read is `usage`.

## Shapes

| Shape | Fields |
| --- | --- |
| `volume-spec` | `name: string`, `uuid: string`, `mount: string`, `root_dir: string`, `availability: string`, `durability: string`, `reserve_bytes: int`, `floor_bytes: int` |
| `volume-state` | `state: string`, `registered?: string`, `found?: string` |
| `volume-status` | `name: string`, `uuid: string`, `mount: string`, `root_dir: string`, `availability: string`, `durability: string`, `reserve_bytes: int`, `floor_bytes: int`, `state: volume-state`, `free_bytes: int?`, `areas: [area-status]` |
| `area-status` | `namespace: string`, `name: string`, `volume: string`, `folder: string`, `absent: absent`, `soft_cap_bytes?: int`, `counts_toward_reserve: bool`, `location?: string`, `fallback: bool`, `roots: int`, `bytes: int`, `stragglers: int`, `unavailable?: string`, `displaced_at?: string` |
| `absent` | `kind: string`, `path?: string` |
| `root` | `id: string`, `label: string`, `path: string`, `folder: string`, `created_unix_ms: int`, `expires_unix_ms?: int` |
| `listed-root` | `id: string`, `label: string`, `path: string`, `created_unix_ms: int`, `last_used_unix_ms: int`, `expires_unix_ms?: int`, `held: bool`, `pinned: bool`, `bytes: int`, `bytes_approximate: bool`, `on_volume: bool` |
| `straggler` | `path: string`, `reason: string` |
| `eviction` | `area: string`, `id: string`, `label: string`, `path: string`, `trash_path: string`, `bytes: int`, `bytes_approximate: bool`, `deleted: bool`, `kept_reason?: string` |
| `prune-report` | `volume: string`, `dry_run: bool`, `free_before_bytes: int`, `free_after_bytes: int`, `freed_bytes: int`, `reserve_bytes: int`, `target_bytes: int`, `floor_bytes: int`, `evicted: [evicted-root]`, `kept: [kept-root]`, `swept: [swept-root]`, `trash_kept: [skipped-root]` |
| `evicted-root` | `area: string`, `id: string`, `label: string`, `path: string`, `bytes: int`, `bytes_approximate: bool`, `reason: string` |
| `kept-root` | `area: string`, `id: string`, `label: string`, `bytes: int`, `bytes_approximate: bool`, `reason: string`, `detail?: string` |
| `swept-root` | `path: string`, `bytes: int` |
| `skipped-root` | `path: string`, `reason: string` |
| `sweep-report` | `deleted: [swept-root]`, `kept: [skipped-root]` |
| `layout` | `schema: string`, `classes: {*: placement}` |
| `placement` | `volume: string`, `path: string`, `absent?: absent` |
| `survey-item` | `path: string`, `classification: string`, `bytes: int?`, `bytes_approximate: bool` |
| `binding-record` | `consumer: string`, `class: string`, `kind: string`, `target: binding-target`, `applied: applied`, `prior: prior` |
| `binding-target` | `kind: string`, `path?: string`, `repository?: string`, `project?: string` |
| `applied` | `value: string`, `at_unix_ms: int`, `identity?: entry-identity` |
| `prior` | `state: string`, `destination?: string`, `path?: string`, `entry?: string` |
| `link-plan` | `consumer: string`, `class: string`, `link: string`, `before: observed`, `after: observed`, `destination_identity: entry-identity`, `replace: bool` |
| `entry-identity` | `dev: int`, `ino: int` |
| `observed` | `state: string`, `destination?: string`, `dev?: int`, `ino?: int` |
| `volume-report` | `name: string`, `mount: string`, `state: volume-state`, `layout: layout-report?` |
| `layout-report` | `agrees: bool`, `classes: [class-folder]`, `unknown: [string]` |
| `class-folder` | `class: string`, `path: string`, `exists: bool` |
| `binding-report` | `consumer: string`, `class: string`, `kind: string`, `path: string`, `state: string`, `observed?: string?`, `expected?: string`, `reason?: string` |
| `misplaced-area` | `area: string`, `recorded: string`, `found: string` |
| `doctor-report` | `ok: bool`, `volume_reports: [volume-report]`, `binding_reports: [binding-report]`, `misplaced_areas: [misplaced-area]`, `worktree_clone_problems?: [worktree-clone-problem]` |
| `move-report` | `plan: move-plan`, `doctor: doctor-report?`, `doctor_error: string?` |
| `move-plan` | `schema: string`, `volume: string`, `volume_spec: volume-spec`, `class: string`, `source: string`, `source_identity: move-identity`, `destination: string`, `destination_parent_identity: move-identity`, `create_directory?: string`, `bindings: [move-binding]`, `unrecorded_links: [unrecorded-link]`, `layout: move-layout?`, `areas: [move-area]`, `consumers: [string]`, `manual_steps: [string]` |
| `move-identity` | `dev: int`, `ino: int` |
| `move-binding` | `record: binding-record`, `before: observed`, `after: string`, `link_after: string` |
| `unrecorded-link` | `location: string`, `link: string`, `before: string`, `after: string`, `consumer: string`, `adopt: string` |
| `move-layout` | `class: string`, `before: placement`, `after: placement` |
| `move-area` | `before: area-record`, `after: area-record` |
| `area-record` | `schema: string`, `namespace: string`, `name: string`, `volume: string`, `folder?: string`, `absent: absent`, `soft_cap_bytes?: int`, `counts_toward_reserve: bool` |
| `machine` | `id?: string`, `name: string`, `hostname: string`, `tailscale_name: string?`, `mac: string?`, `broadcast: string`, `port: int`, `daemon_port: int`, `last_path?: string` |
| `discovered-machine` | `id: string`, `name: string`, `mode: string`, `protocol_version: string`, `address: string`, `port: int`, `registered_as: string?` |
| `paired-client` | `id: string`, `name: string`, `paired_at_unix_ms: int` |
| `machine-state` | `schema: string`, `power: string`, `observed_at_unix_ms: int`, `pending?: pending`, `machine_id: string`, `mac?: string`, `broadcast?: string`, `tailscale_name?: string`, `tailscale_ip?: string` |
| `pending` | `action: string`, `deadline_unix_ms: int` |
| `worktree-lease` | `schema: string`, `lease_id: string`, `request: string`, `repo: string`, `base: string`, `base_name?: string`, `branch: string`, `label?: string`, `pool: string`, `root_id: string`, `path: string`, `head: string`, `warm_kept?: [string]`, `warm_removed?: [string]`, `state: string`, `disposable: bool`, `created_unix_ms: int`, `updated_unix_ms: int` |
| `worktree-pool` | `repo: string`, `pool: string`, `warm: int`, `max: int`, `location?: string`, `unavailable?: string`, `roots: [worktree-root]` |
| `worktree-root` | `id: string`, `path: string`, `held: bool`, `state: string`, `lease_id?: string`, `request?: string`, `branch?: string` |
| `worktree-prune` | `repo: string`, `created: int`, `evicted: [string]`, `finished: [string]` |
| `worktree-clone-problem` | `repo: string`, `path?: string`, `reason: string` |
| `container-lease-status` | `id: string`, `profile: string`, `project: string`, `owner: container-owner`, `class: string`, `ended: container-ending?` |
| `container-owner` | `worktree?: string`, `checkout?: string` |
| `container-ending` | `reason: string`, `remove_volumes: bool` |
| `machine-lease-status` | `id: string`, `machine: string`, `slot: int`, `repo: string`, `owner: container-owner`, `ended: string?` |
| `machine-hold` | `machine: string`, `reason: string?` |
| `run-resolution` | `kind: string`, `machine?: string`, `profile?: string`, `reason?: run-denial` |
| `run-denial` | `kind: string` |

`doctor` and `init --check` can exit 1 while emitting their success document when they detect drift.

A container owner carries exactly one field, `worktree` (a lease ID) or
`checkout` (an absolute path). Lease class is `worker` or `interactive`.
An ending has reason `owner_ended` or `hold_released`. See the [container
lifecycle](../containers.md#lifecycle).
