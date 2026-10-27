#!/usr/bin/env python3
"""Exercise the proof interposer in a fresh fixture without running a native proof."""
import importlib.util
import json
import os
import pathlib
import subprocess
import tempfile
import time

SOURCE = pathlib.Path(__file__).with_name("scratch-enotempty-shim.c")
DRIVER = r'''
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
int main(void) {
    if (rename("target", "captured") != 0) return 1;
    if (unlinkat(AT_FDCWD, "other", AT_REMOVEDIR) != 0) return 2;
    for (int i = 0; i < 3; ++i) {
        errno = 0;
        if (unlinkat(AT_FDCWD, "captured", AT_REMOVEDIR) != -1 || errno != ENOTEMPTY) return 3;
    }
    if (unlinkat(AT_FDCWD, "captured", AT_REMOVEDIR) != 0) return 4;
    return 0;
}
'''


def main():
    scratch = os.environ.get("TMPDIR")
    if not scratch or not os.path.isdir(scratch):
        raise RuntimeError("TMPDIR must name the caller's fresh scratch directory")
    with tempfile.TemporaryDirectory(prefix="shim-test-", dir=scratch) as directory:
        root = pathlib.Path(directory)
        (root / "target").mkdir()
        (root / "other").mkdir()
        (root / "driver.c").write_text(DRIVER)
        library = root / "shim.dylib"
        subprocess.run(["clang", "-dynamiclib", "-Wall", "-Wextra", "-Werror", str(SOURCE), "-o", str(library)], check=True)
        subprocess.run(["clang", "-Wall", "-Wextra", "-Werror", str(root / "driver.c"), "-o", str(root / "driver")], check=True)
        target = root / "target"
        facts = target.stat()
        log = root / "firings.log"
        env = dict(os.environ)
        env.update(DYLD_INSERT_LIBRARIES=str(library), BARGE_PROOF_ENOTEMPTY_PATH=str(target),
                   BARGE_PROOF_ENOTEMPTY_LOG=str(log), BARGE_PROOF_ENOTEMPTY_COUNT="3")
        subprocess.run([str(root / "driver")], env=env, cwd=root, check=True)
        expected = f"{target}\t{facts.st_dev}\t{facts.st_ino}"
        if log.read_text().splitlines() != [expected] * 3:
            raise RuntimeError("the shim did not record exactly three firings for the pinned object")
        if (root / "captured").exists() or (root / "other").exists():
            raise RuntimeError("the shim failed to pass through a non-target or a post-bound removal")
        # The case must observe `removed`, never infer it from `deleted`.
        spec = importlib.util.spec_from_file_location("scratch_proof", SOURCE.with_name("test-scratch-expiry.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        proof = module.Proof("unused", None, False)
        proof.state = str(root / "state")
        journal_dir = root / "state" / "intents"
        journal_dir.mkdir(parents=True)
        journal = journal_dir / "fixture.json"
        def replay_with_removed(_digest, expect):
            journal.write_text(json.dumps({"intent_id": "fixture", "phase": "removed", "generation": 4}))
            time.sleep(0.05)
            journal.write_text(json.dumps({"intent_id": "fixture", "phase": "deleted", "generation": 5}))
            return {"ok": True}
        proof.expire = replay_with_removed
        _result, observed = proof.replay_observing_removed("fixture", "digest")
        if observed != {"phase": "removed", "generation": 4}:
            raise RuntimeError("the watcher did not preserve the observed removed phase")
        proof.expire = lambda _digest, expect: {"ok": True}
        try:
            proof.replay_observing_removed("fixture", "digest")
        except module.Fault:
            pass
        else:
            raise RuntimeError("the watcher inferred removed from an already deleted record")
    print("proof-only ENOTEMPTY interposer: OK")


if __name__ == "__main__":
    main()
