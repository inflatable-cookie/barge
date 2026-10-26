# Barge

Barge moves heavy, rebuildable developer data off your internal drive and onto a dedicated external volume, then keeps it there safely: build output, package caches, scratch, VMs and similar bulk data.

The stage 1 design is frozen in
[docs/knowledge/contracts/stage1-design.md](docs/knowledge/contracts/stage1-design.md)
and is implemented slice by slice: `crates/barge` is the library and
`crates/barge-cli` the thin `barge` binary. See [docs/README.md](docs/README.md).
