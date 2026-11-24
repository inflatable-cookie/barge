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
                                          Everything after -- belongs to CMD: a --json or --help there is passed to
                                          the command untouched and never read by barge.
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

/// Options that never take a value: the word after one is a command word or a
/// positional, never its value. Anything else starting with `--` consumes the
/// next word as its value, unless that word starts with `-` too.
const VALUELESS_OPTIONS: &[&str] = &[
    "apply",
    "bytes",
    "check",
    "confirm-stopped",
    "disposable",
    "dry-run",
    "exclusive",
    "h",
    "help",
    "import-layout",
    "json",
    "no-bytes",
    "plan",
    "replace",
    "retarget-unrecorded",
    "yes",
];

/// The command words in an argument list: option names and their values
/// skipped, so `--help` matches the command no matter where the options sit.
/// Stops at `--`, whose words belong to the child process of `worktree run`.
fn command_words(path: &[String]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut index = 0;
    while index < path.len() {
        let argument = path[index].as_str();
        if argument == "--" {
            break;
        }
        if argument.starts_with('-') && argument != "-" {
            if let Some(name) = argument.strip_prefix("--") {
                let (name, inline) = match name.split_once('=') {
                    Some((name, _)) => (name, true),
                    None => (name, false),
                };
                if !inline && !VALUELESS_OPTIONS.contains(&name) {
                    if let Some(next) = path.get(index + 1) {
                        if !next.starts_with('-') {
                            index += 1;
                        }
                    }
                }
            }
            index += 1;
            continue;
        }
        words.push(argument);
        index += 1;
    }
    words
}

/// The verbs that start a usage entry. A usage line whose first word is one of
/// these starts a new entry; any other indented line continues the entry above
/// it, so multi-line options stay with their command.
const VERBS: &[&str] = &[
    "layout", "path", "env", "survey", "status", "volume", "area", "root", "bind", "move",
    "doctor", "init", "prune", "worktree", "scratch", "machine",
];

fn starts_entry(trimmed: &str) -> bool {
    let first = trimmed.split([' ', '\t']).next().unwrap_or("");
    VERBS.contains(&first)
}

/// The usage text's command lines, grouped with their continuations.
fn entries(usage: &str) -> Vec<Vec<String>> {
    let mut grouped = Vec::new();
    let mut current = Vec::new();
    for line in usage.lines() {
        if line.starts_with(' ') && starts_entry(line.trim_start()) {
            if !current.is_empty() {
                grouped.push(std::mem::take(&mut current));
            }
            current.push(line.to_string());
        } else if line.starts_with(' ') && !line.trim().is_empty() {
            if !current.is_empty() {
                current.push(line.to_string());
            }
        } else if !current.is_empty() {
            grouped.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        grouped.push(current);
    }
    grouped
}

/// The usage `barge <command> --help` prints: the command's own lines,
/// selected from [`usage_text`], so help and the error messages list the same
/// verbs. The `usage:` head names the command it follows; every other line is
/// one of the usage text's own lines. An unknown command falls back to the
/// top-level usage, so `--help` always prints something and never runs.
pub(crate) fn help_for(path: &[String]) -> Option<String> {
    Some(help_for_words(&command_words(path)))
}

fn help_for_words(words: &[&str]) -> String {
    let usage = usage_text();
    let (head, prefix) = match words {
        [] | ["help", ..] => return usage,
        ["worktree", "lease", ..] => (
            "usage: barge worktree lease [--json] <options>",
            "worktree lease",
        ),
        ["worktree", "return", ..] => (
            "usage: barge worktree return [--json] <options>",
            "worktree return",
        ),
        ["worktree", "run", ..] => (
            "usage: barge worktree run [--json] <options> -- CMD [ARGS...]",
            "worktree run",
        ),
        ["worktree", "status", ..] => (
            "usage: barge worktree status [--json] [--repo R]",
            "worktree status",
        ),
        ["worktree", "prune", ..] => (
            "usage: barge worktree prune [--json] [--repo R] [--dry-run]",
            "worktree prune",
        ),
        ["worktree", ..] => ("usage: barge worktree <command>", "worktree "),
        ["scratch", ..] => ("usage: barge scratch <command>", "scratch "),
        ["doctor", ..] => ("usage: barge doctor [--json]", "doctor"),
        ["machine", ..] => ("usage: barge machine <command>", "machine "),
        ["layout", ..] => ("usage: barge layout <command>", "layout "),
        ["path", ..] => ("usage: barge path [--json] <class> [--project P]", "path "),
        ["env", ..] => ("usage: barge env [--shell zsh|bash|fish]", "env "),
        ["survey", ..] => (
            "usage: barge survey [--json] <volume> [--no-bytes]",
            "survey ",
        ),
        ["status", ..] => ("usage: barge status [--json]", "status"),
        ["volume", ..] => ("usage: barge volume [--json] <command>", "volume "),
        ["area", ..] => ("usage: barge area [--json] <command>", "area "),
        ["root", ..] => ("usage: barge root [--json] <command>", "root "),
        ["bind", ..] => ("usage: barge bind [--json] <command>", "bind "),
        ["move", ..] => ("usage: barge move [--json] <options>", "move "),
        ["init", ..] => (
            "usage: barge init [--json] [--project P] [--path DIR] [--check]",
            "init ",
        ),
        ["prune", ..] => (
            "usage: barge prune [--json] --volume V [--dry-run]",
            "prune ",
        ),
        // `help` lists everything, so the whole text is its usage. An unknown
        // command prints the top-level usage, and `--help` exits 0.
        _ => return usage,
    };
    let want = if prefix.ends_with(' ') {
        prefix.to_string()
    } else {
        format!("{prefix} ")
    };
    let mut help = String::from(head);
    help.push_str("\n\n");
    for entry in entries(&usage) {
        let first = entry[0].trim_start();
        if first == prefix || first.starts_with(&want) {
            for line in &entry {
                help.push_str(line);
                help.push('\n');
            }
        }
    }
    help
}

#[cfg(test)]
mod tests {
    use super::{help_for, usage_text};

    fn as_strings(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn every_help_is_a_subset_of_the_usage_text() {
        let usage = usage_text();
        let cases: &[&[&str]] = &[
            &[],
            &["worktree"],
            &["worktree", "lease"],
            &["worktree", "lease", "get"],
            &["worktree", "lease", "--repo", "R"],
            &["worktree", "lease", "get", "--request", "ID"],
            &["worktree", "return", "--lease", "ID"],
            &["worktree", "run", "--lease", "ID", "--", "echo"],
            &["worktree", "status"],
            &["worktree", "status", "--repo", "R"],
            &["worktree", "prune", "--dry-run"],
            &["worktree", "bogus"],
            &["scratch"],
            &["scratch", "new"],
            &["doctor"],
            &["machine"],
            &["machine", "status", "pc"],
            &["layout"],
            &["layout", "show"],
            &["path", "caches"],
            &["env"],
            &["survey", "Builds", "--no-bytes"],
            &["status"],
            &["status", "anything"],
            &["volume"],
            &["volume", "status", "Builds"],
            &["area"],
            &["area", "list"],
            &["root"],
            &["root", "list", "scratch/tests"],
            &["bind"],
            &["bind", "plan"],
            &["move"],
            &["move", "--apply", "--plan", "FILE"],
            &["init", "--check"],
            &["prune", "--volume", "Builds"],
            &["help"],
            &["help", "worktree"],
            &["frobnicate"],
        ];
        for case in cases {
            let help = help_for(&as_strings(case)).expect("help answers every command");
            assert!(!help.is_empty(), "{case:?} prints nothing");
            for line in help.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("usage:") {
                    continue;
                }
                assert!(
                    usage.contains(line),
                    "{case:?} prints a line the usage text lacks: {line:?}"
                );
            }
        }
    }

    #[test]
    fn help_skips_options_and_falls_back_to_the_deepest_command() {
        let lease = help_for(&as_strings(&["worktree", "lease", "--repo", "R"])).unwrap();
        assert!(lease.contains("worktree lease --repo"));
        assert!(!lease.contains("worktree return"));

        let status = help_for(&as_strings(&["worktree", "status", "--repo", "R"])).unwrap();
        assert!(status.contains("worktree status [--repo R]"));
        assert!(!status.contains("worktree lease"));

        let extra = help_for(&as_strings(&["status", "anything"])).unwrap();
        assert!(extra.contains("usage: barge status"));

        let unknown = help_for(&as_strings(&["frobnicate"])).unwrap();
        assert_eq!(unknown, usage_text());
    }
}
