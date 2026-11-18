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
the plan and doctor report.

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
| `barge.doctor/1` | `ok: bool`, `volume_reports: [volume-report]`, `binding_reports: [binding-report]` | doctor |
| `barge.init/1` | `project: string`, `path: string`, `action: string`, `scratch: string`, `volume_name: string`, `volume_state: volume-state` | init |
| `barge.init-check/1` | `project: string`, `path: string`, `block_state: string`, `scratch: string`, `volume_name: string`, `volume_state: volume-state`, `diff: [string]` | init --check |
| `barge.machine/1` | `machine: machine` | machine add |
| `barge.machine-list/1` | `machines: [machine]` | machine list |
| `barge.machine-removed/1` | `removed: machine` | machine remove |
| `barge.machine-wake/1` | `machine_name: string`, `destination: string`, `bytes_sent: int` | machine wake |
| `barge.machine-paired/1` | `machine_name: string` | machine pair |
| `barge.machine-unpaired/1` | `machine_name: string` | machine unpair |
| `barge.machine-status/1` | `machine_name: string`, `mode: string`, `machine_state: machine-state` | machine status |
| `barge.machine-power/1` | `machine_name: string`, `action: string`, `deadline_unix_ms: int` | machine sleep, restart, shutdown |
| `barge.machine-cancel/1` | `machine_name: string`, `cancelled: bool` | machine cancel |

## Error documents

A failing command writes `{"schema": "barge.error/1", "error": {"kind": "...",
"message": "..."}}` to **stderr** in `--json` mode, and nothing to stdout. Human mode prints a plain error to stderr. `kind`
is one of `refused`, `not_found`, `usage`, `io`, `unknown_state`. Exit codes:
0 success, 1 refused or not found (nothing changed), 2 usage error, 3 internal
error. `--json` precedes the command and selects the document form. `env` emits
shell assignments and refuses `--json`.

## Shapes

| Shape | Fields |
| --- | --- |
| `volume-spec` | `name: string`, `uuid: string`, `mount: string`, `root_dir: string`, `availability: string`, `durability: string`, `reserve_bytes: int`, `floor_bytes: int` |
| `volume-state` | `state: string`, `registered?: string`, `found?: string` |
| `volume-status` | `name: string`, `uuid: string`, `mount: string`, `root_dir: string`, `availability: string`, `durability: string`, `reserve_bytes: int`, `floor_bytes: int`, `state: volume-state`, `free_bytes: int?`, `areas: [area-status]` |
| `area-status` | `namespace: string`, `name: string`, `volume: string`, `folder: string`, `absent: absent`, `soft_cap_bytes?: int`, `counts_toward_reserve: bool`, `location?: string`, `fallback: bool`, `roots: int`, `bytes: int`, `stragglers: int`, `unavailable?: string` |
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
| `doctor-report` | `ok: bool`, `volume_reports: [volume-report]`, `binding_reports: [binding-report]` |
| `move-report` | `plan: move-plan`, `doctor: doctor-report` |
| `move-plan` | `schema: string`, `volume: string`, `volume_spec: volume-spec`, `class: string`, `source: string`, `source_identity: move-identity`, `destination: string`, `destination_parent_identity: move-identity`, `create_directory?: string`, `bindings: [move-binding]`, `unrecorded_links: [unrecorded-link]`, `layout: move-layout?`, `areas: [move-area]`, `consumers: [string]`, `manual_steps: [string]` |
| `move-identity` | `dev: int`, `ino: int` |
| `move-binding` | `record: binding-record`, `before: observed`, `after: string`, `link_after: string` |
| `unrecorded-link` | `location: string`, `link: string`, `before: string`, `after: string`, `consumer: string`, `adopt: string` |
| `move-layout` | `class: string`, `before: placement`, `after: placement` |
| `move-area` | `before: area-record`, `after: area-record` |
| `area-record` | `schema: string`, `namespace: string`, `name: string`, `volume: string`, `folder?: string`, `absent: absent`, `soft_cap_bytes?: int`, `counts_toward_reserve: bool` |
| `machine` | `name: string`, `hostname: string`, `tailscale_name: string?`, `mac: string`, `broadcast: string`, `port: int`, `daemon_port: int` |
| `machine-state` | `schema: string`, `power: string`, `observed_at_unix_ms: int`, `pending?: pending` |
| `pending` | `action: string`, `deadline_unix_ms: int` |

`doctor` and `init --check` can exit 1 while emitting their success document when they detect drift.
