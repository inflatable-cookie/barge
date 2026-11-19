#!/usr/bin/env python3
"""Run the installed Northstar check with the account's own skill, never a copy.

The check is Northstar's link check: `effigy skill run --path <skill>
northstar-docs/check-links`. The skill is found the way a named skill is found on
this account (a repository-local Northstar skill first, then the
account's own install), and an ambiguous or missing install refuses rather than
skipping the check.
"""
import argparse
import os
from pathlib import Path
import pwd

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("check", choices=("check-links",))
args = parser.parse_args()
# Match named-skill precedence and ambiguity refusal: the effective account's
# installed skills are authority.
names = ("northstar-docs", "northstar")
local_roots = [Path.cwd() / ".agents/skills" / name for name in names]
account = Path(pwd.getpwuid(os.geteuid()).pw_dir)
local = [path for path in local_roots if (path / "effigy.toml").is_file()]
candidates = local or [
    account / parent / name
    for parent in (".agents/skills", ".codex/skills", ".claude/skills", ".cursor/skills")
    for name in names
]
roots = {path.resolve() for path in candidates if (path / "effigy.toml").is_file()}
if len(roots) != 1:
    parser.error(f"expected one installed Northstar skill, found {len(roots)}; refusing to skip checks")
root = roots.pop()
# The current skill was renamed, including its Effigy catalog alias.
alias = "northstar-docs" if root.name == "northstar-docs" else "northstar"
os.execvp("effigy", ["effigy", "skill", "run", "--path", str(root), f"{alias}/{args.check}"])
