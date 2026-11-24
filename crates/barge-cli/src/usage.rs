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
                                          write AGENTS.local.md, the uncommitted scratch
                                          file; --check reports it and writes nothing
  prune --volume V [--dry-run]            evict idle roots to meet policy
  worktree lease --repo R --base SHA --branch B --request ID
    [--base-name N] [--label L] [--wait SECONDS]
                                          lease a checkout on a new branch
  worktree lease get --request ID | --lease ID
  worktree return --lease ID [--request ID] [--disposable]
                                          reset the checkout and free its root
  worktree run --lease ID [--exclusive] -- CMD [ARGS...]
                                          run CMD in the checkout, holding it
  worktree status [--repo R]              pools, roots and the leases on them
  worktree prune [--repo R] [--dry-run]   maintain each pool’s free checkout target
  scratch new --volume V --project P --label L [--ttl SECONDS]
  scratch sweep --volume V [--project P]  delete expired idle scratch roots
  machine discover [--timeout SECONDS]
  machine add <id-or-name> [--name N] [--timeout SECONDS]
  machine add --name N --hostname H --mac MAC [--tailscale-name N]
    [--broadcast IPv4] [--port PORT] [--daemon-port PORT]
  machine list | remove <name> | wake <name>
  machine pair <name> [--client-name N] | unpair <name>
  machine clients <name> | revoke <name> <client-id>
  machine status <name>                   what barged on that machine reports
  machine sleep <name> [--delay SECONDS]  start the countdown at once
  machine restart <name> [--delay S] [--yes]
  machine shutdown <name> [--delay S] [--yes]
  machine cancel <name>                   stop a pending countdown

SIZES: integer bytes, or KiB, MiB, GiB, TiB.
"
    .to_string()
}

/// The usage `barge <command> --help` prints: the command's own lines from
/// [`usage_text`], so help and the error messages list the same verbs. Every
/// line below is copied from [`usage_text`]; the `usage:` head names the
/// command it follows. Returns `None` for a command that does not exist, so
/// the caller keeps today's unknown-command error.
pub(crate) fn help_for(path: &[String]) -> Option<String> {
    let words: Vec<&str> = path.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => Some(usage_text()),
        ["worktree"] => Some(
            "\
usage: barge worktree <command>

  worktree lease --repo R --base SHA --branch B --request ID
    [--base-name N] [--label L] [--wait SECONDS]
                                          lease a checkout on a new branch
  worktree lease get --request ID | --lease ID
  worktree return --lease ID [--request ID] [--disposable]
                                          reset the checkout and free its root
  worktree run --lease ID [--exclusive] -- CMD [ARGS...]
                                          run CMD in the checkout, holding it
  worktree status [--repo R]              pools, roots and the leases on them
  worktree prune [--repo R] [--dry-run]   maintain each pool’s free checkout target
"
            .to_string(),
        ),
        ["worktree", "lease"] | ["worktree", "lease", "get"] => Some(
            "\
usage: barge worktree lease [--json] <options>

  worktree lease --repo R --base SHA --branch B --request ID
    [--base-name N] [--label L] [--wait SECONDS]
                                          lease a checkout on a new branch
  worktree lease get --request ID | --lease ID
"
            .to_string(),
        ),
        ["worktree", "return"] => Some(
            "\
usage: barge worktree return [--json] <options>

  worktree return --lease ID [--request ID] [--disposable]
                                          reset the checkout and free its root
"
            .to_string(),
        ),
        ["worktree", "run"] => Some(
            "\
usage: barge worktree run [--json] <options> -- CMD [ARGS...]

  worktree run --lease ID [--exclusive] -- CMD [ARGS...]
                                          run CMD in the checkout, holding it

Everything after -- belongs to CMD: a --json or --help there is passed to
the command untouched and never read by barge.
"
            .to_string(),
        ),
        ["worktree", "status"] => Some(
            "\
usage: barge worktree status [--json] [--repo R]

  worktree status [--repo R]              pools, roots and the leases on them
"
            .to_string(),
        ),
        ["worktree", "prune"] => Some(
            "\
usage: barge worktree prune [--json] [--repo R] [--dry-run]

  worktree prune [--repo R] [--dry-run]   maintain each pool’s free checkout target
"
            .to_string(),
        ),
        ["worktree", _] | ["worktree", _, ..] => Some(
            "\
usage: barge worktree <command>

  worktree lease --repo R --base SHA --branch B --request ID
    [--base-name N] [--label L] [--wait SECONDS]
                                          lease a checkout on a new branch
  worktree lease get --request ID | --lease ID
  worktree return --lease ID [--request ID] [--disposable]
                                          reset the checkout and free its root
  worktree run --lease ID [--exclusive] -- CMD [ARGS...]
                                          run CMD in the checkout, holding it
  worktree status [--repo R]              pools, roots and the leases on them
  worktree prune [--repo R] [--dry-run]   maintain each pool’s free checkout target
"
            .to_string(),
        ),
        ["scratch"] | ["scratch", _] | ["scratch", _, ..] => Some(
            "\
usage: barge scratch <command>

  scratch new --volume V --project P --label L [--ttl SECONDS]
  scratch sweep --volume V [--project P]  delete expired idle scratch roots
"
            .to_string(),
        ),
        ["doctor", ..] => Some(
            "\
usage: barge doctor [--json]

  doctor                                  volumes, layout and binding drift
"
            .to_string(),
        ),
        ["machine"] | ["machine", _] | ["machine", _, ..] => Some(
            "\
usage: barge machine <command>

  machine discover [--timeout SECONDS]
  machine add <id-or-name> [--name N] [--timeout SECONDS]
  machine add --name N --hostname H --mac MAC [--tailscale-name N]
    [--broadcast IPv4] [--port PORT] [--daemon-port PORT]
  machine list | remove <name> | wake <name>
  machine pair <name> [--client-name N] | unpair <name>
  machine clients <name> | revoke <name> <client-id>
  machine status <name>                   what barged on that machine reports
  machine sleep <name> [--delay SECONDS]  start the countdown at once
  machine restart <name> [--delay S] [--yes]
  machine shutdown <name> [--delay S] [--yes]
  machine cancel <name>                   stop a pending countdown
"
            .to_string(),
        ),
        ["layout", ..] => Some(
            "\
usage: barge layout <command>

  layout show|set                          inspect or change placements
"
            .to_string(),
        ),
        ["path", ..] => Some(
            "\
usage: barge path [--json] <class> [--project P]

  path <class> [--project P]              resolve a resource path
"
            .to_string(),
        ),
        ["env", ..] => Some(
            "\
usage: barge env [--shell zsh|bash|fish]

  env [--shell zsh|bash|fish]             shell assignments
"
            .to_string(),
        ),
        ["survey", ..] => Some(
            "\
usage: barge survey [--json] <volume> [--no-bytes]

  survey <volume> [--no-bytes]            inspect top-level items
"
            .to_string(),
        ),
        ["status"] => Some(
            "\
usage: barge status [--json]

  status                                  every volume, area and free space
"
            .to_string(),
        ),
        ["volume", ..] => Some(
            "\
usage: barge volume [--json] <command>

  volume register --name N --mount PATH   register a volume (reads its UUID)
    [--uuid U] [--root-dir DIR] [--availability permanent|ephemeral]
    [--durability expendable|kept] [--reserve SIZE] [--floor SIZE] [--import-layout]
  volume list                             registered volumes and their state
  volume status <name>                    one volume's identity and space
"
            .to_string(),
        ),
        ["area", ..] => Some(
            "\
usage: barge area [--json] <command>

  area declare --namespace NS --name A --volume V
    [--folder DIR] [--absent fail|fallback:PATH] [--soft-cap SIZE]
    [--counts-toward-reserve true|false]
  area list [--volume V]                  declared areas and their location
"
            .to_string(),
        ),
        ["root", ..] => Some(
            "\
usage: barge root [--json] <command>

  root list <namespace>/<area> [--bytes]   roots, holds, pins and stragglers
  root pin <namespace>/<area> <id> [--reason TEXT]
  root unpin <namespace>/<area> <id>
  root evict <namespace>/<area> <id>      rename into trash, then delete
"
            .to_string(),
        ),
        ["bind", ..] => Some(
            "\
usage: barge bind [--json] <command>

  bind list                               recorded bindings
  bind plan --consumer C --class K --link PATH --to DEST
                                          the exact before and after; print it
  bind apply --plan FILE|- [--replace]    apply exactly that reviewed plan
  bind adopt --consumer C --class K --link PATH
  bind undo <consumer>                    restore what was there before
"
            .to_string(),
        ),
        ["move", ..] => Some(
            "\
usage: barge move [--json] <options>

  move <class> <new-path> --plan           review a class move
  move --item PATH --into CLASS[/SUB] --plan
  move --apply --plan FILE|- [--confirm-stopped] [--retarget-unrecorded]
"
            .to_string(),
        ),
        ["init", ..] => Some(
            "\
usage: barge init [--json] [--project P] [--path DIR] [--check]

  init [--project P] [--path DIR] [--check]
                                          write AGENTS.local.md, the uncommitted scratch
                                          file; --check reports it and writes nothing
"
            .to_string(),
        ),
        ["prune", ..] => Some(
            "\
usage: barge prune [--json] --volume V [--dry-run]

  prune --volume V [--dry-run]            evict idle roots to meet policy
"
            .to_string(),
        ),
        // `help` lists everything, so the whole text is its usage. An
        // unknown command keeps today's usage error.
        ["help", ..] => Some(usage_text()),
        _ => None,
    }
}
