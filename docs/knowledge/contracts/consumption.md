# Consumption

Rust consumers depend on the `barge` library at an exact 40-hex Git commit,
not a branch or tag. For example, substitute a reviewed pushed-main revision:

```toml
barge = { git = "https://github.com/inflatable-cookie/barge", rev = "<40-hex-commit>" }
```

Commit the consumer's lockfile. Offline builds need the locked Git commit and
its locked dependencies seeded in the consumer's Cargo cache in advance. Oxide
seeds locked Git candidates with its bounded seeder; a missing candidate is an
offline failure, not permission to resolve a moving ref. Tags and releases are
operator-gated.

Non-Rust consumers use the CLI's `barge.*/N` JSON documents. The document
schema name and integer version identify the contract; consumers must check
both and handle unknown versions rather than assuming a compatible shape. The
[stage 1 design](stage1-design.md) owns those formats and versioning rules.

The planner runs `effigy barge:install` or `effigy barge:reinstall` only after
reviewed merge; Tom must authorize the first host install. Set
`BARGE_INSTALL_PIN` to a pushed-main commit and `BARGE_INSTALL_SCRATCH` to a
fresh, empty, private owned scratch directory. The installer creates a separate
private boot-volume pool under `/private/tmp` for Oxide, verifies its device,
identity and socket-path bound, and removes it only after successful release;
an uncertain pool is retained and reported. The installer authenticates the pin
against origin/main, builds its archive offline and locked using Oxide, verifies
the binary's build identity, and installs at `~/.local/bin/barge`. Initial
install refuses an existing PATH entry or destination. Reinstall replaces only
a destination whose hash matches the newest receipt in
`~/.local/state/barge/install/`; every successful run retains earlier receipts.
