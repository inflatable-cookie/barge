//! The usage text: the one place the CLI's verbs, options and command synopses
//! are listed for a person. `docs/knowledge/architecture.md#cli` owns the
//! verb list itself. Per-command help prints a `usage:` header line from this
//! text plus that command's own entry lines, so help cannot drift from it.

/// The usage text `barge`, `barge help` and a usage error print.
pub(crate) fn usage_text() -> String {
    "\
barge — storage volumes, areas, roots, holds, pins, pruning and scratch

usage: barge [--json] <command>

usage: barge layout <command>
  layout show|set                          inspect or change placements
usage: barge path [--json] <class> [--project P]
  path <class> [--project P]              resolve a resource path
usage: barge env [--shell zsh|bash|fish]
  env [--shell zsh|bash|fish]             shell assignments
usage: barge survey [--json] <volume> [--bytes] [--no-bytes]
  survey <volume> [--bytes] [--no-bytes]  classify top-level items; --no-bytes is deprecated
                                          --bytes walks every non-system item; it can take minutes on a large drive
usage: barge status [--json]
  status                                  every volume, area and free space
usage: barge volume [--json] <command>
  volume register --name N --mount PATH   register a volume (reads its UUID)
    [--uuid U] [--root-dir DIR] [--availability permanent|ephemeral]
    [--durability expendable|kept] [--reserve SIZE] [--floor SIZE] [--import-layout]
  volume list                             registered volumes and their state
  volume status <name>                    one volume's identity and space
usage: barge area [--json] <command>
  area declare --namespace NS --name A --volume V
    [--folder DIR] [--absent fail|fallback:PATH] [--soft-cap SIZE]
    [--counts-toward-reserve true|false]
  area list [--volume V]                  declared areas and their location
usage: barge root [--json] <command>
  root list <namespace>/<area> [--bytes]   roots, holds, pins and stragglers
  root pin <namespace>/<area> <id> [--reason TEXT]
  root unpin <namespace>/<area> <id>
  root evict <namespace>/<area> <id>      rename into trash, then delete
usage: barge bind [--json] <command>
  bind list                               recorded bindings
  bind plan --consumer C --class K --link PATH --to DEST
                                          the exact before and after; print it
  bind apply --plan FILE|- [--replace]    apply exactly that reviewed plan
  bind adopt --consumer C --class K --link PATH
  bind undo <consumer>                    restore what was there before
usage: barge move [--json] <options>
  move <class> <new-path> --plan           review a class move
  move --item PATH --into CLASS[/SUB] --plan
  move --apply --plan FILE|- [--confirm-stopped] [--retarget-unrecorded]
usage: barge doctor [--json]
  doctor                                  volumes, layout and binding drift
usage: barge init [--json] [--project P] [--path DIR] [--check]
  init [--project P] [--path DIR] [--check]
                                          write AGENTS.local.md, the uncommitted scratch
                                          file; --check reports it and writes nothing
usage: barge prune [--json] --volume V [--dry-run]
  prune --volume V [--dry-run]            evict idle roots to meet policy
usage: barge worktree <command>
usage: barge worktree lease [--json] <options>
  worktree lease --repo R --base SHA --branch B --request ID
    [--base-name N] [--label L] [--wait SECONDS]
                                          lease a checkout on a new branch
usage: barge worktree lease get [--json] <options>
  worktree lease get --request ID | --lease ID
usage: barge worktree return [--json] <options>
  worktree return --lease ID [--request ID] [--disposable]
                                          reset the checkout and free its root
usage: barge worktree run [--json] <options> -- CMD [ARGS...]
  worktree run --lease ID [--exclusive] -- CMD [ARGS...]
                                          run CMD in the checkout, holding it
                                          Everything after -- belongs to CMD: a --json or --help there is passed to
                                          the command untouched and never read by barge.
usage: barge worktree status [--json] [--repo R]
  worktree status [--repo R]              pools, roots and the leases on them
usage: barge worktree prune [--json] [--repo R] [--dry-run]
  worktree prune [--repo R] [--dry-run]   maintain each pool’s free checkout target
usage: barge container <command>
usage: barge container status [--json]
  container status                      leases and their endings
usage: barge container return [--json] <id>
  container return <id>                 release a lease after stopping its stack
usage: barge run <command>
usage: barge run resolve [--json]
  run resolve                           preview placement for a JSON request on stdin
usage: barge scratch <command>
  scratch new --volume V --project P --label L [--ttl SECONDS]
  scratch sweep --volume V [--project P]  delete expired idle scratch roots
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
/// it, so multi-line options stay with their command. `usage:` headers and
/// unindented lines separate entries instead.
const VERBS: &[&str] = &[
    "layout",
    "path",
    "env",
    "survey",
    "status",
    "volume",
    "area",
    "root",
    "bind",
    "move",
    "doctor",
    "init",
    "prune",
    "worktree",
    "container",
    "run",
    "scratch",
    "machine",
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

/// The command path a `usage: barge <path> ...` header names, or `None` for
/// the top-level head, whose words after `barge` are an option and a
/// placeholder rather than a command.
fn header_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("usage: barge ")?;
    let end = rest
        .find(" [")
        .or_else(|| rest.find(" <"))
        .unwrap_or(rest.len());
    let path = rest[..end].trim_end();
    if path.is_empty() || path.contains(['[', '<']) {
        return None;
    }
    Some(path)
}

/// The usage `barge <command> --help` prints: that command's `usage:` header
/// line from [`usage_text`] plus its own entry lines, so help and the error
/// messages list the same verbs and no synopsis lives anywhere else. The
/// header is found by the longest command-word prefix that names one, so an
/// unknown command falls back to the top-level usage and `--help` always
/// prints something without running.
pub(crate) fn help_for(path: &[String]) -> Option<String> {
    Some(help_for_words(&command_words(path)))
}

fn help_for_words(words: &[&str]) -> String {
    let usage = usage_text();
    let mut matched: Option<(&str, String)> = None;
    for length in (1..=words.len()).rev() {
        let path = words[..length].join(" ");
        if let Some(head) = usage
            .lines()
            .find(|line| header_path(line) == Some(path.as_str()))
        {
            matched = Some((head, path));
            break;
        }
    }
    let Some((head, path)) = matched else {
        return usage;
    };
    let want = format!("{path} ");
    let mut help = String::from(head);
    help.push_str("\n\n");
    for entry in entries(&usage) {
        let first = entry[0].trim_start();
        if first == path || first.starts_with(&want) {
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
    use super::{header_path, help_for, usage_text};

    fn as_strings(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn every_help_line_comes_from_the_usage_text() {
        let usage = usage_text();
        let lines: Vec<&str> = usage.lines().collect();
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
            &["container"],
            &["container", "status"],
            &["container", "return", "ID"],
            &["run"],
            &["run", "resolve"],
            &["scratch"],
            &["scratch", "new"],
            &["doctor"],
            &["machine"],
            &["machine", "status", "pc"],
            &["layout"],
            &["layout", "show"],
            &["path", "caches"],
            &["env"],
            &["survey", "Builds", "--bytes"],
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
                if line.trim().is_empty() {
                    continue;
                }
                assert!(
                    lines.contains(&line),
                    "{case:?} prints a line the usage text lacks: {line:?}"
                );
            }
        }
    }

    #[test]
    fn each_command_prints_its_own_synopsis_header() {
        let cases: &[(&[&str], &str)] = &[
            (&["worktree"], "usage: barge worktree <command>"),
            (
                &["worktree", "lease"],
                "usage: barge worktree lease [--json] <options>",
            ),
            (
                &["worktree", "lease", "get"],
                "usage: barge worktree lease get [--json] <options>",
            ),
            (
                &["worktree", "return"],
                "usage: barge worktree return [--json] <options>",
            ),
            (
                &["worktree", "run"],
                "usage: barge worktree run [--json] <options> -- CMD [ARGS...]",
            ),
            (
                &["worktree", "status"],
                "usage: barge worktree status [--json] [--repo R]",
            ),
            (
                &["worktree", "prune"],
                "usage: barge worktree prune [--json] [--repo R] [--dry-run]",
            ),
            (&["container"], "usage: barge container <command>"),
            (
                &["container", "status"],
                "usage: barge container status [--json]",
            ),
            (
                &["container", "return", "ID"],
                "usage: barge container return [--json] <id>",
            ),
            (&["run"], "usage: barge run <command>"),
            (&["run", "resolve"], "usage: barge run resolve [--json]"),
            (&["scratch"], "usage: barge scratch <command>"),
            (&["doctor"], "usage: barge doctor [--json]"),
            (&["machine"], "usage: barge machine <command>"),
            (&["layout"], "usage: barge layout <command>"),
            (
                &["path"],
                "usage: barge path [--json] <class> [--project P]",
            ),
            (&["env"], "usage: barge env [--shell zsh|bash|fish]"),
            (
                &["survey"],
                "usage: barge survey [--json] <volume> [--bytes] [--no-bytes]",
            ),
            (&["status"], "usage: barge status [--json]"),
            (&["volume"], "usage: barge volume [--json] <command>"),
            (&["area"], "usage: barge area [--json] <command>"),
            (&["root"], "usage: barge root [--json] <command>"),
            (&["bind"], "usage: barge bind [--json] <command>"),
            (&["move"], "usage: barge move [--json] <options>"),
            (
                &["init"],
                "usage: barge init [--json] [--project P] [--path DIR] [--check]",
            ),
            (
                &["prune"],
                "usage: barge prune [--json] --volume V [--dry-run]",
            ),
        ];
        for (case, head) in cases {
            let help = help_for(&as_strings(case)).expect("help answers every command");
            assert_eq!(help.lines().next(), Some(*head), "{case:?}");
            assert!(
                usage_text().lines().any(|line| line == *head),
                "{head:?} lives in the usage text"
            );
        }
    }

    #[test]
    fn headers_name_command_paths_and_nothing_else() {
        assert_eq!(header_path("usage: barge [--json] <command>"), None);
        assert_eq!(
            header_path("usage: barge worktree lease [--json] <options>"),
            Some("worktree lease")
        );
        assert_eq!(
            header_path("usage: barge worktree <command>"),
            Some("worktree")
        );
        assert_eq!(header_path("  worktree lease --repo R --base SHA"), None);
    }

    #[test]
    fn help_skips_options_and_falls_back_to_the_deepest_command() {
        let lease = help_for(&as_strings(&["worktree", "lease", "--repo", "R"])).unwrap();
        assert!(lease.contains("worktree lease --repo"));
        assert!(!lease.contains("worktree return"));

        let get = help_for(&as_strings(&["worktree", "lease", "get"])).unwrap();
        assert!(get.contains("usage: barge worktree lease get"));
        assert!(get.contains("worktree lease get"));
        assert!(
            !get.contains("worktree lease --repo"),
            "the get leaf excludes the creation line: {get}"
        );

        let status = help_for(&as_strings(&["worktree", "status", "--repo", "R"])).unwrap();
        assert!(status.contains("worktree status [--repo R]"));
        assert!(!status.contains("worktree lease"));

        let extra = help_for(&as_strings(&["status", "anything"])).unwrap();
        assert!(extra.contains("usage: barge status"));

        let unknown = help_for(&as_strings(&["frobnicate"])).unwrap();
        assert_eq!(unknown, usage_text());
    }
}
