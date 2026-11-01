#!/usr/bin/env python3
"""Bounded, report-only Builds preflight. No Barge effect form is callable."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile

MOUNT = Path('/Volumes/Builds')
PARENT = MOUNT / 'scratch/barge'
LIMIT = 1024 * 1024


def allowed(args):
    # Exact shapes, rather than a blacklist of destructive verbs. Values are
    # supplied by this program, not forwarded from the command line.
    if args in (['describe', '--json'], ['space', '--json'],
                ['maintain', '--space', '--json']):
        return True
    return (len(args) == 17 and args[:4] == ['volume', 'register', '--mount', str(MOUNT)]
            and args[4:8] == ['--availability', 'permanent', '--durability', 'expendable']
            and args[8:10] == ['--root-dir', 'barge']
            and args[10] == '--reserve' and args[11].isdigit()
            and args[12:14] == ['--floor', str(10 * 1024**3)]
            and args[14:] == ['--declared-by', 't9-qualification', '--plan'])


def barge(binary, args):
    if not allowed(args):
        raise ValueError('effect or unsupported command refused')
    return command([binary] + args + ([] if '--json' in args else ['--json']))


def command(argv):
    result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            timeout=60, check=False)
    if len(result.stdout) > LIMIT or len(result.stderr) > LIMIT:
        raise RuntimeError('output exceeded 1 MiB')
    return {'argv': argv, 'exit': result.returncode,
            'stdout': result.stdout.decode('utf-8', 'replace'),
            'stderr': result.stderr.decode('utf-8', 'replace')}


def self_check():
    for args in (['volume', 'register', '--plan-file', 'x', '--plan-sha256', 'x'],
                 ['volume', 'update', 'x', '--durability', 'kept'],
                 ['space', 'prune', '--plan', 'x', '--plan-sha256', 'x'],
                 ['space', 'guard'], ['maintain', '--space', '--plan', 'x', '--plan-sha256', 'x'],
                 ['scratch', 'claim'], ['space', 'prune', '--plan']):
        try:
            barge('/never-executed', args)
        except ValueError:
            continue
        raise AssertionError(args)
    assert allowed(['space', '--json'])
    print('effect forms refused')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--self-check', action='store_true')
    parser.add_argument('--binary')
    opts = parser.parse_args()
    if opts.self_check:
        self_check()
        return
    if not opts.binary or not Path(opts.binary).is_absolute():
        parser.error('--binary must name an absolute Barge executable')
    # Do not create or follow a missing/symlink scratch parent.
    if not PARENT.is_dir() or PARENT.is_symlink() or PARENT.resolve() != PARENT:
        raise RuntimeError('expected existing, direct Builds scratch/barge directory')
    packet = Path(tempfile.mkdtemp(prefix='t9-preview-', dir=PARENT))
    results = {}
    def record(name, value):
        (packet / (name + '.json')).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')
    try:
        results['binary'] = {'path': opts.binary, 'sha256': hashlib.sha256(Path(opts.binary).read_bytes()).hexdigest()}
        info = subprocess.run(['/usr/sbin/diskutil', 'info', '-plist', str(MOUNT)], capture_output=True, timeout=60, check=True)
        if len(info.stdout) > LIMIT:
            raise RuntimeError('diskutil output exceeded bound')
        facts = plistlib.loads(info.stdout)
        results['volume'] = {key: facts.get(key) for key in ('VolumeUUID', 'MountPoint', 'FilesystemType', 'TotalSize', 'FreeSpace', 'DeviceIdentifier')}
        if facts.get('MountPoint') != str(MOUNT) or not facts.get('VolumeUUID'):
            raise RuntimeError('Builds volume identity unavailable')
        fs = os.statvfs(MOUNT)
        free, total = fs.f_bavail * fs.f_frsize, fs.f_blocks * fs.f_frsize
        results['statvfs'] = {'free_bytes': free, 'total_bytes': total, 'block_bytes': fs.f_frsize}
        results['df'] = command(['/bin/df', '-k', str(MOUNT)])
        results['oxide_pool'] = command(['/Users/tom/.local/bin/oxide', 'pool', 'describe', '--json'])
        results['presence'] = {}
        for name in ('oxide-pool', 'oxide-outputs', 'scratch', 'barge', 'barge/scratch'):
            p = MOUNT / name
            try:
                s = p.lstat()
                results['presence'][name] = {'mode': s.st_mode, 'device': s.st_dev, 'inode': s.st_ino}
            except FileNotFoundError:
                results['presence'][name] = None
        results['describe'] = barge(opts.binary, ['describe', '--json'])
        results['space'] = barge(opts.binary, ['space', '--json'])
        results['maintain'] = barge(opts.binary, ['maintain', '--space', '--json'])
        floor = 10 * 1024**3
        if free <= 2 * floor:
            raise RuntimeError('insufficient free space to choose preview thresholds')
        reserve = min(100 * 1024**3, free // 2)
        results['thresholds'] = {'reserve_bytes': reserve, 'floor_bytes': floor,
                                 'rationale': 'reserve is the lesser of 100 GiB and half measured available bytes; floor is the 10 GiB default'}
        results['register_plan'] = barge(opts.binary, ['volume', 'register', '--mount', str(MOUNT), '--availability', 'permanent', '--durability', 'expendable', '--root-dir', 'barge', '--reserve', str(reserve), '--floor', str(floor), '--declared-by', 't9-qualification', '--plan'])
    except Exception as exc:
        results['failure'] = {'error': str(exc)}
    for name, value in results.items():
        record(name, value)
    files = sorted(packet.glob('*.json'))
    (packet / 'SHA256SUMS').write_text(''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n' for p in files))
    print(packet)
    if 'failure' in results or any(v.get('exit', 0) != 0 for v in results.values() if isinstance(v, dict)):
        sys.exit(1)


if __name__ == '__main__':
    main()
