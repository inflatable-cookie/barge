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

Move plans are `barge.move-plan/1` documents, printed directly by `barge move
--plan` and supplied unchanged to `barge move --apply --plan`. Successful JSON
apply output is a `barge.move/1` envelope containing the plan and doctor report.

Except for standalone move plans, every success document has `schema` and `data`. The fields of `data` are:

| Schema | Fields | Commands |
| --- | --- | --- |
| `barge.status/1` | `volumes` | status |
| `barge.volume-registered/1` | `volume`, `state` | volume register |
| `barge.volume-list/1` | `volumes` | volume list |
| `barge.volume/1` | `volume`, `state`, `free_bytes` | volume status |
| `barge.area-declared/1` | `area` | area declare |
| `barge.areas/1` | `areas` | area list |
| `barge.roots/1` | `area`, `bytes_included`, `roots`, `stragglers` | root list |
| `barge.root-action/1` | `area`, `id`, `action` | root pin, unpin |
| `barge.root-evicted/1` | `action`, `eviction` | root evict |
| `barge.prune/1` | `prune` | prune |
| `barge.scratch/1` | `area`, `root` | scratch new |
| `barge.sweep/1` | `sweep` | scratch sweep |
| `barge.layout-show/1` | `layout`, `disagreements` | layout show |
| `barge.layout-updated/1` | `layout` | layout set |
| `barge.path/1` | `path` | path |
| `barge.survey/1` | `volume`, `items` | survey |
| `barge.binding-list/1` | `bindings` | bind list |
| `barge.bind-plan/1` | `plan` | bind plan |
| `barge.binding/1` | `action`, `binding` | bind apply, adopt, undo |
| `barge.move/1` | `report` | move apply |
| `barge.doctor/1` | `ok`, `volumes`, `bindings` | doctor |
| `barge.machine/1` | `machine` | machine add |
| `barge.machine-list/1` | `machines` | machine list |
| `barge.machine-removed/1` | `removed` | machine remove |
| `barge.machine-wake/1` | `machine`, `destination`, `bytes_sent` | machine wake |
| `barge.machine-status/1` | `machine`, `state` | machine status |
| `barge.machine-power/1` | `machine`, `action`, `deadline_unix_ms` | machine sleep, restart, shutdown |
| `barge.machine-cancel/1` | `machine`, `cancelled` | machine cancel |

Errors have `schema: "barge.error/1"` and an `error` object with `kind` and `message` fields. `env` emits shell assignments and does not support JSON. The global
`--json` option precedes the command.
