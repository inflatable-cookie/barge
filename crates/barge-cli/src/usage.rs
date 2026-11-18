//! The usage text: the one place the CLI's verbs and options are listed for a
//! person. `docs/knowledge/architecture.md#cli` owns the verb list itself.

/// The usage text `barge`, `barge help` and a usage error print.
pub(crate) fn usage_text() -> String {
    "\
barge — storage volumes, areas, roots, holds, pins, pruning and scratch

usage: barge [--json] <command>

  layout show|set                          inspect or change placements
  path <class> [--project P]              resolve a resource path
  env [--shell zsh|bash|fish]             shell assignments
  survey <volume> [--no-bytes]            inspect top-level items
  status                                  every volume, area and free space
  volume register --name N --mount PATH   register a volume (reads its UUID)
    [--uuid U] [--root-dir DIR] [--availability permanent|ephemeral]
    [--durability expendable|kept] [--reserve SIZE] [--floor SIZE] [--import-layout]
  volume list                             registered volumes and their state
  volume status <name>                    one volume's identity and space
  area declare --namespace NS --name A --volume V
    [--folder DIR] [--absent fail|fallback:PATH] [--soft-cap SIZE]
    [--counts-toward-reserve true|false]
  area list [--volume V]                  declared areas and their location
  root list <namespace>/<area> [--bytes]   roots, holds, pins and stragglers
  root pin <namespace>/<area> <id> [--reason TEXT]
  root unpin <namespace>/<area> <id>
  root evict <namespace>/<area> <id>      rename into trash, then delete
  bind list                               recorded bindings
  bind plan --consumer C --class K --link PATH --to DEST
                                          the exact before and after; print it
  bind apply --plan FILE|- [--replace]    apply exactly that reviewed plan
  bind adopt --consumer C --class K --link PATH
  bind undo <consumer>                    restore what was there before
  move <class> <new-path> --plan           review a class move
  move --item PATH --into CLASS[/SUB] --plan
  move --apply --plan FILE|- [--confirm-stopped] [--retarget-unrecorded]
  doctor                                  volumes, layout and binding drift
  init [--project P] [--path DIR] [--check]
                                          write the AGENTS.md scratch block;
                                          --check reports it and writes nothing
  prune --volume V [--dry-run]            evict idle roots to meet policy
  scratch new --volume V --project P --label L [--ttl SECONDS]
  scratch sweep --volume V [--project P]  delete expired idle scratch roots
  machine add --name N --hostname H --mac MAC [--tailscale-name N]
    [--broadcast IPv4] [--port PORT] [--daemon-port PORT]
  machine list | remove <name> | wake <name>
  machine pair <name> | unpair <name>
  machine status <name>                   what barged on that machine reports
  machine sleep <name> [--delay SECONDS]  start the countdown at once
  machine restart <name> [--delay S] [--yes]
  machine shutdown <name> [--delay S] [--yes]
  machine cancel <name>                   stop a pending countdown

SIZES: integer bytes, or KiB, MiB, GiB, TiB.
"
    .to_string()
}
