# Barge

Barge moves heavy, rebuildable developer data off your internal drive and onto a dedicated external volume, then keeps it there safely: build output, package caches, scratch, VMs and similar bulk data.

The stage 1 design is frozen in
[docs/knowledge/contracts/stage1-design.md](docs/knowledge/contracts/stage1-design.md)
and is implemented slice by slice: `crates/barge` is the library and
`crates/barge-cli` the thin `barge` binary. See [docs/README.md](docs/README.md).

Volume policy is declared explicitly. Preview a registration with
`barge volume register --mount <mount-root> --availability permanent --durability expendable --root-dir <folder> --reserve 20GiB --declared-by <name> --plan --json`.
The preview's `data.plan` is the exact UTF-8 JSON text to save in a file and
`data.plan_sha256` is its digest. Publish it with
`barge volume register --plan-file <file> --plan-sha256 <digest> --json`.
`barge volume update <uuid> --reserve 24GiB --declared-by <name> --plan --json`
produces an update plan, applied with the same options on `volume update`.
`barge volume describe <uuid> --json` reports the record. A plan refuses if
the registry or volume changes before publication. Sizes are integer bytes or
integers with KiB, MiB, GiB or TiB suffixes; the floor defaults to 10 GiB for registration and is retained on update. An
update of an undeclared policy must explicitly supply both thresholds. An absent policy remains undeclared;
Oxide's reserve is never imported. These commands write Barge account state only.
