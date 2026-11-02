#!/usr/bin/env python3
"""Export a locked default-feature tape executable from an exact Git tree.

Run only inside the separately qualified private Oxide wrapper. This producer
is not an isolation qualification: the wrapper and resource gates precede it.
No fixture or evaluator window is run. Every failed premise refuses export.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile

SCRATCH = Path('/Volumes/Builds/scratch/barge').resolve()
HEX40 = re.compile(r'^[0-9a-f]{40}$')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def command(args, cwd, env):
    result = subprocess.run(args, cwd=cwd, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, check=False)
    if result.returncode:
        raise RuntimeError(f'{args[0]} exited {result.returncode}: '
                           + result.stderr.decode('utf-8', 'replace')[-4000:])
    return result.stdout


def inventory(root, members):
    result = {}
    for name, kind in members.items():
        path = root / name
        if kind == 'symlink':
            if not path.is_symlink():
                raise RuntimeError(f'changed source type: {name}')
            data = os.fsencode(os.readlink(path))
        else:
            if path.is_symlink() or not path.is_file():
                raise RuntimeError(f'changed source type: {name}')
            data = path.read_bytes()
        result[name] = {'kind': kind, 'sha256': digest(data)}
    return result


def main():
    if len(sys.argv) != 4:
        raise RuntimeError('usage: export.py REPOSITORY COMMIT EMPTY_OWNED_ROOT')
    repo, commit, root = Path(sys.argv[1]).resolve(), sys.argv[2], Path(sys.argv[3]).resolve()
    if not HEX40.fullmatch(commit):
        raise RuntimeError('an exact 40-digit commit is required')
    if not root.is_relative_to(SCRATCH) or root == SCRATCH or not root.is_dir() or any(root.iterdir()):
        raise RuntimeError('export root must be a fresh empty owned directory under Barge scratch')
    # An external qualified wrapper must supply these, after its positive dry
    # isolation and resource gates. This check is not itself isolation evidence.
    oxide = Path(os.environ['BARGE_PRIVATE_OXIDE']).resolve(strict=True)
    rustc = Path(os.environ['BARGE_PINNED_RUSTC']).resolve(strict=True)
    private_root = Path(os.environ['BARGE_PRIVATE_ROOT']).resolve(strict=True)
    if not private_root.is_relative_to(SCRATCH) or not oxide.is_relative_to(private_root):
        raise RuntimeError('Oxide is not inside the owned private root')
    if not root.is_relative_to(private_root):
        raise RuntimeError('export is not inside the owned private root')
    if os.environ.get('BARGE_PRIVATE_WRAPPER_QUALIFIED') != '1':
        raise RuntimeError('qualified private wrapper is required')
    env = dict(os.environ)
    if (Path(shutil.which('oxide') or '/nonexistent').resolve() != oxide or
            Path(env.get('RUSTC', '/nonexistent')).resolve() != rustc or
            env.get('RUSTUP_TOOLCHAIN') != '1.97.1'):
        raise RuntimeError('effective Oxide or Rust toolchain binding mismatch')
    tree = command(['git', 'rev-parse', commit + '^{tree}'], repo, env).decode().strip()
    resolved = command(['git', 'rev-parse', commit + '^{commit}'], repo, env).decode().strip()
    if resolved != commit or not HEX40.fullmatch(tree):
        raise RuntimeError('commit or tree resolution mismatch')
    archive = command(['git', 'archive', '--format=tar', commit], repo, env)
    source = root / 'source'
    source.mkdir()
    members = {}
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:') as tar:
        for item in tar:
            parts = Path(item.name).parts
            if not parts or any(p in ('', '.', '..') for p in parts) or Path(item.name).is_absolute():
                raise RuntimeError('unsafe archive name')
            path = source.joinpath(*parts)
            if item.isdir():
                path.mkdir(parents=True, exist_ok=True)
            elif item.isfile():
                path.parent.mkdir(parents=True, exist_ok=True)
                stream = tar.extractfile(item)
                if stream is None or item.size > 64 * 1024 * 1024:
                    raise RuntimeError('invalid source file')
                path.write_bytes(stream.read())
                os.chmod(path, item.mode & 0o777)
                members[item.name] = 'file'
            elif item.issym():
                target = Path(item.linkname)
                if target.is_absolute() or '..' in target.parts:
                    raise RuntimeError('unsafe source symlink')
                path.parent.mkdir(parents=True, exist_ok=True)
                path.symlink_to(item.linkname)
                members[item.name] = 'symlink'
            else:
                raise RuntimeError('unsupported archive member')
    before = inventory(source, members)
    if (before.get('scripts/tape/export.py', {}).get('sha256') != digest(Path(__file__).read_bytes()) or
            before.get('scripts/tape/private-wrapper.py', {}).get('sha256') !=
            env.get('BARGE_PRIVATE_WRAPPER_SHA256')):
        raise RuntimeError('producer or wrapper does not match the archived commit')
    for required in ('Cargo.lock', 'rust-toolchain.toml'):
        if required not in before:
            raise RuntimeError(f'missing {required}')
    version = command([str(rustc), '--version'], source, env).decode().strip()
    if not version.startswith('rustc 1.97.1 '):
        raise RuntimeError('wrong rustc version')
    output = command(['effigy', 'rust:test:tape-stage'], source, env)
    if inventory(source, members) != before:
        raise RuntimeError('source changed during build')
    observed = {p.relative_to(source).as_posix() for p in source.rglob('*')
                if p.is_file() or p.is_symlink()}
    if observed != set(members):
        raise RuntimeError('source gained or lost files during build')
    artifacts = []
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if (item.get('reason') == 'compiler-artifact' and
                item.get('target', {}).get('name') == 'tape_eval' and
                item.get('executable') and not item.get('features')):
            artifacts.append(Path(item['executable']).resolve(strict=True))
    if len(artifacts) != 1 or not artifacts[0].is_relative_to(private_root):
        raise RuntimeError('no unique default-feature private tape executable')
    exported = root / 'tape_eval'
    shutil.copyfile(artifacts[0], exported)
    os.chmod(exported, 0o700)
    receipt = {'schema': 'barge.tape.export/1', 'commit': commit, 'tree': tree,
               'files': before, 'lock_sha256': before['Cargo.lock']['sha256'],
               'toolchain_sha256': before['rust-toolchain.toml']['sha256'],
               'rustc_sha256': digest(rustc.read_bytes()), 'rustc_version': version,
               'oxide_sha256': digest(oxide.read_bytes()),
               'executable_sha256': digest(exported.read_bytes())}
    data = (json.dumps(receipt, sort_keys=True, separators=(',', ':')) + '\n').encode()
    (root / 'export.json').write_bytes(data)
    print(digest(data))


if __name__ == '__main__':
    try:
        main()
    except (KeyError, OSError, RuntimeError, ValueError) as error:
        print(f'tape export refused: {error}', file=sys.stderr)
        sys.exit(2)
