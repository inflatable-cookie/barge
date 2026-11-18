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
