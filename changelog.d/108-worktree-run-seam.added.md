Worktree pools: `barge worktree run --lease <id> [--exclusive] -- <command>` holds a leased checkout while a non-Rust host's command runs in it, and refuses a return meanwhile.
