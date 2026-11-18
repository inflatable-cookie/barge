#!/usr/bin/env python3
"""Run the installed Northstar check under a fixture HOME without losing skill discovery."""
import argparse
import os
from pathlib import Path
import pwd

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("check", choices=("check-links",))
args = parser.parse_args()
# Match named-skill precedence and ambiguity refusal. The effective account's
# installed skills are authority; HOME may deliberately be a private fixture.
local = Path.cwd() / ".agents/skills/northstar"
account = Path(pwd.getpwuid(os.geteuid()).pw_dir)
candidates = [local] if local.is_dir() else [
    account / ".agents/skills/northstar", account / ".codex/skills/northstar",
    account / ".claude/skills/northstar", account / ".cursor/skills/northstar",
]
roots = {path.resolve() for path in candidates if (path / "effigy.toml").is_file()}
if len(roots) != 1:
    parser.error(f"expected one installed Northstar skill, found {len(roots)}; refusing to skip checks")
os.execvp("effigy", ["effigy", "skill", "run", "--path", str(roots.pop()), f"northstar/{args.check}"])
