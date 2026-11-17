# Barge

Barge moves heavy, rebuildable developer data off your internal drive and onto a
dedicated external volume, and keeps it there safely: build output, caches,
scratch and similar bulk data.

`crates/barge` is the library that holds every mechanic;
`crates/barge-cli` is the thin `barge` binary over it, with `--json` output for
other tools. Read [docs/README.md](docs/README.md) and the
[architecture](docs/knowledge/architecture.md).

```sh
barge volume register --name Builds --mount /Volumes/Builds --reserve 20GiB
barge area declare --namespace oxide --name pool --volume Builds \
  --absent fallback:/Users/me/Library/Caches/oxide-pool
barge root list oxide/pool
barge prune --volume Builds --dry-run
barge scratch new --volume Builds --project effigy --label test-run --ttl 3600
barge status --json
```

`area declare` records the declaration the library and later callers must match;
the CLI needs it because `root list` and the other root verbs work on declared
areas. Exit codes: 0 success, 1 refused (nothing changed), 2 usage error,
3 internal error.
