#!/usr/bin/env python3
"""Exercise the proof interposer in a fresh fixture without running a native proof."""
import importlib.util
import json
import os
import pathlib
import signal
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

PAUSE_DRIVER = r'''
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
int main(void) {
    if (rename("pause-unit", "quarantined") != 0) return 1;
    int other = open("pass-unit", O_RDONLY | O_DIRECTORY);
    if (other < 0 || unlinkat(other, "other", 0) != 0 || close(other) != 0) return 2;
    int unit = open("quarantined", O_RDONLY | O_DIRECTORY);
    if (unit < 0) return 3;
    return unlinkat(unit, "payload", 0) == 0 ? 0 : 4;
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
        # Pause only an unlink below the pinned directory after it is renamed.
        (root / "pause-unit").mkdir()
        (root / "pause-unit" / "payload").write_text("keep")
        (root / "pass-unit").mkdir()
        (root / "pass-unit" / "other").write_text("pass")
        (root / "pause-driver.c").write_text(PAUSE_DRIVER)
        subprocess.run(["clang", "-Wall", "-Wextra", "-Werror", str(root / "pause-driver.c"), "-o", str(root / "pause-driver")], check=True, timeout=30)
        pause_target = root / "pause-unit"
        pause_facts = pause_target.stat()
        pause_log = root / "pause.log"
        pause_env = dict(os.environ)
        pause_env.update(DYLD_INSERT_LIBRARIES=str(library), BARGE_PROOF_PAUSE_PATH=str(pause_target),
                         BARGE_PROOF_PAUSE_LOG=str(pause_log))
        for key in ("BARGE_PROOF_ENOTEMPTY_PATH", "BARGE_PROOF_ENOTEMPTY_LOG", "BARGE_PROOF_ENOTEMPTY_COUNT"):
            pause_env.pop(key, None)
        process = subprocess.Popen([str(root / "pause-driver")], cwd=root, env=pause_env, preexec_fn=os.setpgrp)
        try:
            deadline = time.monotonic() + 5
            while not pause_log.exists() and process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.01)
            if not pause_log.exists():
                raise RuntimeError("the exact fixture pause was not reached")
            expected_pause = f"{pause_target}\t{pause_facts.st_dev}\t{pause_facts.st_ino}"
            if pause_log.read_text().splitlines() != [expected_pause] or process.poll() is not None:
                raise RuntimeError("the pause did not pin and hold the renamed unit")
            if not (root / "quarantined" / "payload").exists() or (root / "pass-unit" / "other").exists():
                raise RuntimeError("the pause target or unrelated pass-through was wrong")
            os.kill(process.pid, signal.SIGUSR1)
            if process.wait(timeout=5) != 0 or (root / "quarantined" / "payload").exists():
                raise RuntimeError("the released pause did not resume the original unlink")
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        # The case must observe `removed`, never infer it from `deleted`.
        spec = importlib.util.spec_from_file_location("scratch_proof", SOURCE.with_name("test-scratch-expiry.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        evidence_dir = root / "evidence"
        evidence_dir.mkdir()
        backing = root / "backing"
        backing.write_bytes(b"fixture backing")
        retained = pathlib.Path(module.retain_backing(str(backing), str(evidence_dir), str(root)))
        if retained.stat().st_ino != backing.stat().st_ino:
            raise RuntimeError("retained backing does not share the original inode")
        backing.unlink()
        if retained.read_bytes() != b"fixture backing":
            raise RuntimeError("retained backing was lost with the original name")
        retained.unlink()
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
    print("proof-only ENOTEMPTY and pause interposer: OK")


if __name__ == "__main__":
    main()
