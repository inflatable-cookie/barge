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
import shutil
import admission

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
            if time.monotonic() - started > 30 or len(rows) >= 50000:
                raise RuntimeError('inventory bound reached')
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
                        if time.monotonic() - started > 30:
                            raise RuntimeError('inventory time bound reached')
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
    started = time.monotonic()
    if len(sys.argv) != 8 or sys.argv[1] not in ('dry', 'validate', 'export'):
        raise RuntimeError('usage: private-wrapper.py dry|validate|export PRIVATE_ROOT REPOSITORY COMMIT EXPORT_ROOT DEBUG_OXIDE_RECEIPT EXPECTED_RECEIPT_SHA256')
    mode, private, repo, commit, export, receipt, expected = sys.argv[1:]
    requested_private = Path(private)
    if requested_private.is_symlink():
        raise RuntimeError('private root must not be a symlink')
    private = requested_private.resolve(strict=True)
    if private == SCRATCH or not private.is_relative_to(SCRATCH):
        raise RuntimeError('private root must be under Barge scratch')
    private_stat = private.stat()
    if not stat.S_ISDIR(private_stat.st_mode) or private_stat.st_uid != os.geteuid() or private_stat.st_mode & 0o077:
        raise RuntimeError('private root must be owned and inaccessible to other users')
    requested_block = Path(os.environ['BARGE_VALIDATION_BLOCK'])
    if requested_block.is_symlink():
        raise RuntimeError('authorization block must not be a symlink')
    block_path = requested_block.resolve(strict=True)
    if not block_path.is_relative_to(SCRATCH):
        raise RuntimeError('authorization block must be a retained private file')
    block_digest = os.environ['BARGE_VALIDATION_BLOCK_SHA256']
    block = admission.read_json_bound(block_path, block_digest)
    if block.get('schema') != 'barge.validation-block/1' or block.get('task') != admission.TASK:
        raise RuntimeError('authorization block does not name this task')
    # A fresh block is consumed once, including failing preflight work. Never
    # delete the claim or start a new clock for this same authorization.
    claim = block_path.with_name(block_path.name + '.claimed')
    with claim.open('x', encoding='utf-8') as out:
        json.dump({'block_id': block.get('block_id'), 'head': commit,
                   'block_sha256': block_digest}, out, sort_keys=True)
    receipt_path = Path(receipt)
    if receipt_path.is_symlink() or receipt_path.resolve(strict=True) != receipt_path or not receipt_path.is_relative_to(private):
        raise RuntimeError('debug Oxide receipt must be inside the private root')
    data = receipt_path.read_bytes()
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
    if binding['source_commit'] != admission.OXIDE_COMMIT or binding['source_tree'] != admission.OXIDE_TREE:
        raise RuntimeError('debug Oxide source is not the pinned snapshot')
    source = (private / 'oxide-source').resolve(strict=True)
    if not source.is_relative_to(private):
        raise RuntimeError('debug Oxide source escapes the private root')
    files = binding.get('source_files')
    if not isinstance(files, dict) or not files:
        raise RuntimeError('debug Oxide source file inventory missing')
    for rel, expected_hash in files.items():
        if not isinstance(rel, str):
            raise RuntimeError('debug Oxide source path is not a string')
        path = source / rel
        if (Path(rel).is_absolute() or '..' in Path(rel).parts
                or path.is_symlink() or path.resolve(strict=True) != path or not path.is_file() or not isinstance(expected_hash, str)
                or sha(path.read_bytes()) != expected_hash):
            raise RuntimeError('debug Oxide source file binding mismatch')
    if (files.get('Cargo.lock') != binding['lock_sha256'] or
            files.get('rust-toolchain.toml') != binding['toolchain_sha256']):
        raise RuntimeError('debug Oxide lock or toolchain binding mismatch')
    build = binding.get('build')
    if not isinstance(build, dict) or build.get('command') != ['cargo', 'build', '--offline', '--locked', '--bin', 'oxide', '--message-format=json'] \
            or build.get('exit_code') != 0 \
            or build.get('artifact_sha256') != sha(oxide.read_bytes()) \
            or build.get('source_tree') != binding['source_tree'] \
            or build.get('rustc_sha256') != binding['rustc_sha256'] \
            or build.get('cwd') != str(source) \
            or build.get('lock_sha256') != binding['lock_sha256'] \
            or build.get('toolchain_sha256') != binding['toolchain_sha256']:
        raise RuntimeError('source-bound debug build and closure evidence required')
    for name in ('log_sha256', 'supervisor_receipt_sha256'):
        if not isinstance(build.get(name), str) or not admission.HEX64.fullmatch(build[name]):
            raise RuntimeError('debug build evidence digest missing')
    for name, digest_name in (('log_path', 'log_sha256'), ('supervisor_receipt_path', 'supervisor_receipt_sha256')):
        evidence = Path(build.get(name, '')).resolve(strict=True)
        if not evidence.is_relative_to(private) or sha(evidence.read_bytes()) != build[digest_name]:
            raise RuntimeError('debug build evidence is absent or changed')
    build_supervisor = json.loads(Path(build['supervisor_receipt_path']).read_text(encoding='utf-8'))
    admission.verify_supervisor(build_supervisor, build['command'], private)
    artifacts = []
    for line in Path(build['log_path']).read_bytes().splitlines():
        try:
            item = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
        target = item.get('target', {})
        if (item.get('reason') == 'compiler-artifact' and target.get('name') == 'oxide'
                and 'bin' in target.get('kind', [])
                and target.get('src_path') == str(source / 'src/bin/oxide/main.rs')
                and item.get('executable')):
            artifacts.append(Path(item['executable']).resolve(strict=True))
    if (len(artifacts) != 1 or not artifacts[0].is_relative_to(private)
            or sha(artifacts[0].read_bytes()) != sha(oxide.read_bytes())):
        raise RuntimeError('debug build log does not bind the staged executable')
    # A successful compiler is not isolation evidence. Bind and compare the
    # actual pre/post inventories; the retained failed run cannot satisfy this.
    before_evidence = Path(build.get('live_before_path', '')).resolve(strict=True)
    after_evidence = Path(build.get('live_after_path', '')).resolve(strict=True)
    if not before_evidence.is_relative_to(private) or not after_evidence.is_relative_to(private):
        raise RuntimeError('debug build inventory evidence escapes private root')
    before_bytes, after_bytes = before_evidence.read_bytes(), after_evidence.read_bytes()
    if (sha(before_bytes) != build.get('live_before_sha256')
            or sha(after_bytes) != build.get('live_after_sha256') or before_bytes != after_bytes):
        raise RuntimeError('debug build isolation inventory did not close')
    supervisor = Path(os.environ['BARGE_VALIDATION_SUPERVISOR']).resolve(strict=True)
    if not supervisor.is_relative_to(private) or sha(supervisor.read_bytes()) != admission.SUPERVISOR_SHA256:
        raise RuntimeError('qualified supervisor binding missing')
    effigy = admission.direct_effigy(Path(os.environ['BARGE_VALIDATION_EFFIGY']))
    git = Path(os.environ['BARGE_VALIDATION_GIT']).resolve(strict=True)
    audit_path = Path(os.environ['BARGE_VALIDATION_SOURCE_AUDIT'])
    if audit_path.is_symlink() or audit_path.resolve(strict=True) != audit_path or not audit_path.is_relative_to(SCRATCH):
        raise RuntimeError('source audit must be a retained private file')
    audit_digest = os.environ['BARGE_VALIDATION_SOURCE_AUDIT_SHA256']
    audit = admission.read_json_bound(audit_path, audit_digest)
    admission.validate_audit(audit, private=private, effigy_hash=sha(effigy.read_bytes()),
                             oxide_hash=sha(oxide.read_bytes()), wrapper_hash=sha(Path(__file__).read_bytes()),
                             git_hash=sha(git.read_bytes()), rustc_hash=sha(rustc.read_bytes()),
                             supervisor_hash=sha(supervisor.read_bytes()))
    ledger_path = Path(os.environ['BARGE_VALIDATION_ROOT_LEDGER'])
    if ledger_path.is_symlink() or ledger_path.resolve(strict=True) != ledger_path or not ledger_path.is_relative_to(SCRATCH):
        raise RuntimeError('root ledger must be a retained private file')
    ledger_digest = os.environ['BARGE_VALIDATION_ROOT_LEDGER_SHA256']
    ledger = admission.read_json_bound(ledger_path, ledger_digest)
    roots = admission.validate_ledger(ledger, private)
    for evidence_path in (block_path, audit_path, ledger_path):
        if not any(evidence_path == root or root in evidence_path.parents for root in roots):
            raise RuntimeError('authorization or audit evidence is not counted by the root ledger')
    limit = admission.validate_block(block, head=commit, wrapper_hash=sha(Path(__file__).read_bytes()),
                                     oxide_hash=sha(oxide.read_bytes()), audit_hash=audit_digest,
                                     ledger_hash=ledger_digest)
    if ((mode == 'dry' and len(block['selectors']) != 1)
            or (mode == 'validate' and (len(block['selectors']) < 2 or 'rust:test:tape-export' in block['selectors']))
            or (mode == 'export' and block['selectors'][-1] != 'rust:test:tape-export')):
        raise RuntimeError('mode and authorized selector sequence disagree')
    admission_env = admission.host_admission(os.environ, audit)
    env = admission.child_environment(os.environ, private=private, oxide=oxide, rustc=rustc,
                                      wrapper_hash=sha(Path(__file__).read_bytes()),
                                      pool_cap=block['pool_cap_bytes'], admission_env=admission_env)
    build_env = build.get('environment')
    expected_build_env = {key: env[key] for key in ('HOME', 'CARGO_HOME', 'TMPDIR', 'RUSTUP_HOME',
                          'RUSTUP_TOOLCHAIN', 'RUSTC', 'OXIDE_CONFIG', 'OXIDE_POOL',
                          'OXIDE_SCRATCH_ROOT', 'OXIDE_TASK_SCRATCH_ROOT',
                          'OXIDE_TEST_HOME_FROM_ENV', 'CARGO_NET_OFFLINE')}
    if build_env != expected_build_env:
        raise RuntimeError('debug build environment binding is missing or mismatched')
    if Path(shutil.which('effigy', path=env['PATH']) or '/nonexistent').resolve() != effigy:
        raise RuntimeError('child Effigy differs from the audited binary')
    if Path(shutil.which('oxide', path=env['PATH']) or '/nonexistent').resolve() != oxide:
        raise RuntimeError('child Oxide differs from the audited binary')
    cargo = Path(shutil.which('cargo', path=env['PATH']) or '/nonexistent').resolve(strict=True)
    if (audit.get('child_path') != env['PATH'] or audit.get('cargo_sha256') != sha(cargo.read_bytes())
            or build.get('cargo_sha256') != sha(cargo.read_bytes())
            or audit.get('python_sha256') != sha(Path(sys.executable).resolve(strict=True).read_bytes())):
        raise RuntimeError('child PATH, Cargo or Python differs from source audit')
    rustup_home = Path(env['RUSTUP_HOME']).resolve(strict=True)
    if not rustc.is_relative_to(rustup_home):
        raise RuntimeError('pinned rustc is outside the selected RUSTUP_HOME')
    for name in ('home', 'cache', 'tmp', 'scratch'):
        path = (private / name).resolve(strict=True)
        if not path.is_relative_to(private) or not path.is_dir():
            raise RuntimeError('private destination is missing or escaped')
    config = (private / 'config.toml').resolve(strict=True)
    if not config.is_relative_to(private) or not config.is_file():
        raise RuntimeError('private config is missing or escaped')
    other_roots = [root for root in roots if root != private]
    other_bytes = admission.allocated_bytes(other_roots)
    admission.allocated_bytes(roots)
    remaining_mib = (admission.CAP_BYTES - other_bytes) // (1024 * 1024)
    if remaining_mib < 1:
        raise RuntimeError('retained evidence leaves no private scratch budget')
    # The source tree is a separately prepared git-archive, never the checkout.
    work = (private / 'barge-source').resolve(strict=True)
    if not work.is_relative_to(private) or not (work / 'effigy.toml').is_file():
        raise RuntimeError('isolated Barge source is missing')
    if sha((work / 'scripts/tape/private-wrapper.py').read_bytes()) != sha(Path(__file__).read_bytes()):
        raise RuntimeError('isolated source wrapper differs')
    if sha((work / 'scripts/tape/admission.py').read_bytes()) != sha(Path(admission.__file__).read_bytes()):
        raise RuntimeError('isolated admission policy differs')

    def save_inventory(name, rows):
        with (private / name).open('x', encoding='utf-8') as out:
            json.dump(rows, out, sort_keys=True, separators=(',', ':'))

    source_command_count = 0
    def run_source(command, child_env):
        nonlocal source_command_count
        source_command_count += 1
        label = f'tape-source-{source_command_count}'
        remaining = limit - (time.monotonic() - started)
        if remaining <= 0:
            raise RuntimeError('authorized covering block expired during preflight')
        before = inventory()
        save_inventory(label + '-before.json', before)
        result = None
        try:
            result = subprocess.run(command, cwd=private, env=child_env,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    timeout=remaining, check=False)
        finally:
            after = inventory()
            save_inventory(label + '-after.json', after)
            admission.allocated_bytes(roots)
            admission.verify_inventory(before, after)
        if result is None or result.returncode or len(result.stdout) > 64 * 1024 * 1024:
            raise RuntimeError('bounded source verification did not succeed')
        return result.stdout

    oxide_origin = Path(os.environ['BARGE_OXIDE_ORIGIN']).resolve(strict=True)
    actual_oxide_files = admission.verify_git_tree(oxide_origin, source, admission.OXIDE_COMMIT,
                                                   admission.OXIDE_TREE, git, run_source)
    if actual_oxide_files != files:
        raise RuntimeError('debug Oxide archive and build receipt disagree')
    admission.verify_git_tree(Path(repo).resolve(strict=True), work, commit,
                              block['barge_tree'], git, run_source)
    source_snapshot = admission.source_snapshot(work)
    oxide_snapshot = admission.source_snapshot(source)

    dry_path = private / 'tape-dry-inventory.json'
    identity = {'oxide': sha(oxide.read_bytes()), 'config': sha(config.read_bytes()),
                'receipt': sha(data), 'rustc': sha(rustc.read_bytes()), 'private': str(private)}
    # Fixture sentinels are unique private files. Their appearance in recorded
    # status demonstrates that the actual resolver read private state and cache.
    home = private / 'home'
    state = home / 'Library/Application Support/oxide/state'
    cache = home / 'Library/Caches/oxide'
    for directory in (state, cache):
        admission.private_directory(private, directory.relative_to(private))
    registry_probe = private / ('registry-probe-' + block['block_id'])
    cache_probe = cache / 'build-pool'
    for path in (registry_probe, cache_probe):
        admission.private_create(private, path.relative_to(private), block['block_id'].encode())
    admission.private_create(private, (state / 'pools.json').relative_to(private),
                             json.dumps([str(registry_probe)]).encode())
    admission.allocated_bytes(roots)

    def run_checked(selector, arguments, label):
        remaining = int(limit - (time.monotonic() - started))
        if remaining < 60:
            raise RuntimeError('authorized covering block has insufficient remaining time')
        before = inventory()
        save_inventory(label + '-before.json', before)
        evidence = private / (label + '-supervisor')
        command = [str(effigy), selector] + arguments
        supervised = [sys.executable, str(supervisor), '--limit', str(remaining),
                      '--scratch-budget-mib', str(remaining_mib), '--evidence', str(evidence),
                      '--'] + command
        stdout_path = private / (label + '-stdout.log')
        stderr_path = private / (label + '-stderr.log')
        result = None
        try:
            with stdout_path.open('xb') as stdout, stderr_path.open('xb') as stderr:
                result = subprocess.run(supervised, cwd=work, env=env, check=False,
                                        stdout=stdout, stderr=stderr)
        finally:
            after = inventory()
            save_inventory(label + '-after.json', after)
            admission.allocated_bytes(roots)
            admission.verify_inventory(before, after)
            if admission.source_snapshot(work) != source_snapshot or admission.source_snapshot(source) != oxide_snapshot:
                raise RuntimeError('source archive changed during a supervised child')
        if result is None or result.returncode:
            raise RuntimeError(f'{selector} did not close successfully')
        receipt_doc = json.loads((evidence / 'receipt.json').read_text(encoding='utf-8'))
        admission.verify_supervisor(receipt_doc, command, private)
        if stdout_path.stat().st_size > 8 * 1024 * 1024 or stderr_path.stat().st_size > 8 * 1024 * 1024:
            raise RuntimeError('selector output exceeds retained evidence bound')
        return stdout_path.read_text(encoding='utf-8')

    status = run_checked('rust:test:tape-private-status', [], 'tape-dry')
    admission.verify_probe(status, home=home, config=config, pool=private / 'scratch/pool',
                           registry_probe=registry_probe, cache_probe=cache_probe)
    identity.update({'schema': 'barge.validation-dry/1', 'block_sha256': block_digest,
                     'audit_sha256': audit_digest, 'ledger_sha256': ledger_digest,
                     'head': commit, 'barge_tree': block['barge_tree'],
                     'status_sha256': sha(status.encode('utf-8')),
                     'live_before_sha256': sha((private / 'tape-dry-before.json').read_bytes()),
                     'live_after_sha256': sha((private / 'tape-dry-after.json').read_bytes())})
    if identity['live_before_sha256'] != identity['live_after_sha256']:
        raise RuntimeError('serialized dry inventories differ')
    with dry_path.open('x', encoding='utf-8') as out:
        json.dump(identity, out, sort_keys=True)
    env['BARGE_TAPE_DRY_RECEIPT'] = str(dry_path)
    env['BARGE_TAPE_DRY_SHA256'] = sha(dry_path.read_bytes())
    if mode == 'dry':
        return
    for index, selector in enumerate(block['selectors'][1:]):
        arguments = ['--', repo, commit, export] if selector == 'rust:test:tape-export' else []
        run_checked(selector, arguments, f'tape-command-{index}')
    if mode == 'validate':
        return
    export_root = Path(export).resolve(strict=True)
    if not export_root.is_relative_to(private):
        raise RuntimeError('export root escaped the private root')
    export_receipt = (export_root / 'export.json').read_bytes()
    closure = {
        'schema': 'barge.tape.export-closure/1', 'status': 'passed',
        'scope': 'live-inventory-comparison-only',
        'receipt_sha256': sha(export_receipt),
        'wrapper_sha256': sha(Path(__file__).read_bytes()),
        'live_before_sha256': sha((private / ('tape-command-' + str(len(block['selectors']) - 2) + '-before.json')).read_bytes()),
        'live_after_sha256': sha((private / ('tape-command-' + str(len(block['selectors']) - 2) + '-after.json')).read_bytes()),
    }
    if closure['live_before_sha256'] != closure['live_after_sha256']:
        raise RuntimeError('serialized live inventories differ')
    closure_bytes = (json.dumps(closure, sort_keys=True, separators=(',', ':')) + '\n').encode()
    with (export_root / 'export-closure.json').open('xb') as out:
        out.write(closure_bytes)
    print(f'export-closure-sha256={sha(closure_bytes)}')


if __name__ == '__main__':
    try:
        main()
    except (KeyError, OSError, ValueError, RuntimeError, UnicodeError, subprocess.TimeoutExpired) as error:
        print(f'private wrapper refused: {error}', file=sys.stderr)
        sys.exit(2)
