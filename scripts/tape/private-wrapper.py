#!/usr/bin/env python3
"""Private Barge tape-export launcher. No execution is qualified by its presence.

Requires an independently source-bound debug Oxide receipt and a prior dry
inventory receipt. The live inventory brackets each child; any change refuses.
The retained bootstrap failure is not repaired or attributed by this wrapper.
"""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time

SCRATCH = Path('/Volumes/Builds/scratch/barge').resolve()
LIVE = Path('/Users/tom/Library/Application Support/oxide')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def inventory():
    rows = []
    total = 0
    started = time.monotonic()
    nofollow = os.O_NOFOLLOW
    directory = os.O_DIRECTORY

    def stable(st):
        return (st.st_dev, st.st_ino, st.st_mode, st.st_size, st.st_mtime_ns)

    def walk(fd, rel):
        nonlocal total
        if time.monotonic() - started > 30 or len(rows) >= 50000:
            raise RuntimeError('inventory bound reached')
        before = os.fstat(fd)
        rows.append((rel, stable(before)))
        with os.scandir(fd) as entries:
            names = sorted(entry.name for entry in entries)
        for name in names:
            st = os.stat(name, dir_fd=fd, follow_symlinks=False)
            child = rel + '/' + name
            if stat.S_ISDIR(st.st_mode):
                sub = os.open(name, os.O_RDONLY | nofollow | directory, dir_fd=fd)
                try:
                    if stable(st) != stable(os.fstat(sub)):
                        raise RuntimeError('inventory directory replaced')
                    walk(sub, child)
                finally:
                    os.close(sub)
            elif stat.S_ISREG(st.st_mode):
                total += st.st_size
                if total > 268435456 or len(rows) >= 50000:
                    raise RuntimeError('inventory bound reached')
                sub = os.open(name, os.O_RDONLY | nofollow, dir_fd=fd)
                try:
                    if stable(st) != stable(os.fstat(sub)):
                        raise RuntimeError('inventory file replaced')
                    h = hashlib.sha256()
                    while True:
                        data = os.read(sub, 1048576)
                        if not data:
                            break
                        h.update(data)
                    if stable(st) != stable(os.fstat(sub)):
                        raise RuntimeError('inventory file changed')
                finally:
                    os.close(sub)
                rows.append((child, stable(st), h.hexdigest()))
            elif stat.S_ISLNK(st.st_mode):
                rows.append((child, stable(st), os.readlink(name, dir_fd=fd)))
            else:
                raise RuntimeError('unsupported inventory entry')
            if stable(st) != stable(os.stat(name, dir_fd=fd, follow_symlinks=False)):
                raise RuntimeError('inventory entry changed')
        if stable(before) != stable(os.fstat(fd)):
            raise RuntimeError('inventory directory changed')

    try:
        fd = os.open(LIVE, os.O_RDONLY | nofollow | directory)
    except FileNotFoundError:
        raise RuntimeError('live inventory root missing; isolation unproven')
    try:
        walk(fd, '.')
    finally:
        os.close(fd)
    return rows


def main():
    if len(sys.argv) != 8 or sys.argv[1] not in ('dry', 'export'):
        raise RuntimeError('usage: private-wrapper.py dry|export PRIVATE_ROOT REPOSITORY COMMIT EXPORT_ROOT DEBUG_OXIDE_RECEIPT EXPECTED_RECEIPT_SHA256')
    mode, private, repo, commit, export, receipt, expected = sys.argv[1:]
    # The retained failure leaves admission subprocess isolation and aggregate
    # scratch accounting unproved. A matching inventory alone cannot qualify
    # those premises. This source must be completed and independently reviewed
    # before it can authorize even a dry gate or invoke the selector.
    raise RuntimeError('private admission and aggregate resource gates are not established')
    private = Path(private).resolve(strict=True)
    if private == SCRATCH or not private.is_relative_to(SCRATCH):
        raise RuntimeError('private root must be under Barge scratch')
    data = Path(receipt).read_bytes()
    if sha(data) != expected:
        raise RuntimeError('debug Oxide receipt digest mismatch')
    binding = json.loads(data)
    oxide = (private / 'bin/oxide').resolve(strict=True)
    if not oxide.is_relative_to(private) or binding.get('executable_sha256') != sha(oxide.read_bytes()):
        raise RuntimeError('debug Oxide executable binding mismatch')
    if binding.get('profile') != 'debug' or binding.get('test_home_from_env') is not True:
        raise RuntimeError('debug account-control binding missing')
    for key in ('source_commit', 'source_tree', 'lock_sha256', 'toolchain_sha256', 'rustc_sha256'):
        if not isinstance(binding.get(key), str) or len(binding[key]) not in (40, 64):
            raise RuntimeError('incomplete debug Oxide provenance')
    rustc = Path(os.environ['BARGE_PINNED_RUSTC']).resolve(strict=True)
    if sha(rustc.read_bytes()) != binding['rustc_sha256']:
        raise RuntimeError('rustc binding mismatch')
    if os.environ.get('RUSTUP_TOOLCHAIN') != '1.97.1':
        raise RuntimeError('pinned toolchain is required')
    env = {k: os.environ[k] for k in ('RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'PATH') if k in os.environ}
    env.update({'HOME': str(private / 'home'), 'CARGO_HOME': str(private / 'cache'),
                'TMPDIR': str(private / 'tmp'), 'TMP': str(private / 'tmp'),
                'TEMP': str(private / 'tmp'), 'OXIDE_CONFIG': str(private / 'config.toml'),
                'OXIDE_POOL': str(private / 'pool'), 'OXIDE_TASK_SCRATCH_ROOT': str(private / 'scratch'),
                'OXIDE_TEST_HOME_FROM_ENV': '1', 'BARGE_PRIVATE_ROOT': str(private),
                'BARGE_PRIVATE_OXIDE': str(oxide), 'BARGE_PINNED_RUSTC': str(rustc),
                'BARGE_PRIVATE_WRAPPER_QUALIFIED': '1', 'RUSTC': str(rustc),
                'BARGE_PRIVATE_WRAPPER_SHA256': sha(Path(__file__).read_bytes()),
                'CARGO_NET_OFFLINE': 'true', 'RUSTUP_AUTO_INSTALL': '0',
                'PATH': str(private / 'bin') + os.pathsep + env.get('PATH', '')})
    for name in ('home', 'cache', 'tmp', 'pool', 'scratch'):
        path = (private / name).resolve(strict=True)
        if not path.is_relative_to(private) or not path.is_dir():
            raise RuntimeError('private destination is missing or escaped')
    config = (private / 'config.toml').resolve(strict=True)
    if not config.is_relative_to(private) or not config.is_file():
        raise RuntimeError('private config is missing or escaped')
    before = inventory()
    def save_inventory(name, rows):
        with (private / name).open('x', encoding='utf-8') as out:
            json.dump(rows, out, sort_keys=True, separators=(',', ':'))

    dry_path = private / 'tape-dry-inventory.json'
    identity = {'oxide': sha(oxide.read_bytes()), 'config': sha(config.read_bytes()),
                'receipt': sha(data), 'rustc': sha(rustc.read_bytes()), 'private': str(private)}
    if mode == 'dry':
        save_inventory('tape-dry-before.json', before)
        after = inventory()
        save_inventory('tape-dry-after.json', after)
        if after != before:
            raise RuntimeError('dry inventory changed')
        dry_path.write_text(json.dumps(identity, sort_keys=True), encoding='utf-8')
        return
    if json.loads(dry_path.read_text(encoding='utf-8')) != identity:
        raise RuntimeError('dry isolation gate is missing or changed')
    save_inventory('tape-export-before.json', before)
    # Effigy is the admission point. No raw Cargo command or native proof is run.
    try:
        result = subprocess.run(['effigy', 'rust:test:tape-export', '--', repo, commit, export],
                                cwd=repo, env=env, check=False)
    finally:
        after = inventory()
        save_inventory('tape-export-after.json', after)
        if before != after:
            raise RuntimeError('live inventory changed; isolation failed')
    if result.returncode:
        raise RuntimeError(f'export selector exited {result.returncode}')


if __name__ == '__main__':
    try:
        main()
    except (KeyError, OSError, ValueError, RuntimeError) as error:
        print(f'private wrapper refused: {error}', file=sys.stderr)
        sys.exit(2)
