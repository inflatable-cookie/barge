"""Source-only policy for a future, separately authorized validation block.

A digest received from Queue/operator is the trust anchor for the block and
its source audit. This module never creates a block and grants no authority.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat

SCRATCH = Path('/Volumes/Builds/scratch/barge')
TASK = '9f435fb3-4374-4f23-ba6d-7c5e6869f4ae'
OXIDE_COMMIT = '3de241325dd286ae44a5759fa6825b05a81c7b4e'
OXIDE_TREE = '452a9c2dc6b1432c643c5053655e029955977754'
SUPERVISOR_SHA256 = 'fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b'
CAP_BYTES = 3 * 1024 * 1024 * 1024
HEX64 = re.compile(r'^[0-9a-f]{64}$')
CANDIDATE_SCHEMA = 'oxide.private-build-validation.candidate/1'
REQUIRED_AUDIT = frozenset(('oxide_config', 'oxide_registry', 'oxide_scratch',
                            'oxide_child', 'effigy_admission', 'rustup_cargo',
                            'compiler_linker', 'build_scripts', 'git_source', 'retained_roots'))
SAFE_SELECTORS = frozenset(('rust:compile', 'rust:compile:release', 'rust:format',
                            'rust:lint', 'rust:test:tape:pure', 'rust:test:tape-stage',
                            'rust:test:tape-export', 'rust:test:tape-export:pure',
                            'rust:test:tape-admission:pure', 'docs:check'))


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_json_bound(path, expected):
    if not isinstance(expected, str) or not HEX64.fullmatch(expected):
        raise ValueError('independent digest required')
    with open(path, 'rb') as source:
        data = source.read(8 * 1024 * 1024 + 1)
    if len(data) > 8 * 1024 * 1024 or digest(data) != expected:
        raise ValueError('document digest or bound mismatch')
    value = json.loads(data)
    if not isinstance(value, dict):
        raise ValueError('document is not an object')
    return value


def read_candidate_document(path, expected, *, text=False):
    if not isinstance(expected, str) or not HEX64.fullmatch(expected):
        raise ValueError('independent evidence digest required')
    with open(path, 'rb') as source:
        data = source.read(8 * 1024 * 1024 + 1)
    if len(data) > 8 * 1024 * 1024 or digest(data) != expected:
        raise ValueError('candidate evidence missing, changed or over bound')
    return data.decode('utf-8') if text else json.loads(data)


COMPONENT_SHA256 = '0bd4f39b88d3042089545fd8b6a166496ffdfe2fa7d8c2acf71f26d66761a79b'
PROVIDER_SHA256 = '26e0f82e4278d3322c8ddc8343cbe6bd18b106a92aa9b7ca2c3c04953f8abd3c'
SEEDER_SHA256 = '5cbf42e127eb07c0eecbe96e82f47b72c64b144e3f0a922e45ec1bdcd66e374d'
COMPONENT_COMMIT = '2f64d5aae0b9de93cba283d6b7be53a511e88ad6'


def candidate_runtime(candidate, *, component_path, component_hash, component_commit,
                      docs, digests, context, issuance_path, block, loader=None):
    """Compose the canonical Oxide facts with Barge's own admission bounds."""
    path = Path(component_path)
    provider_path = path.with_name('test-private-build-validation.py')
    if component_commit != COMPONENT_COMMIT or component_hash != COMPONENT_SHA256:
        raise ValueError('unreviewed candidate component identity')
    for source, expected in ((path, COMPONENT_SHA256), (provider_path, PROVIDER_SHA256)):
        if (not source.is_absolute() or source.is_symlink() or source.resolve(strict=True) != source
                or not source.is_file() or digest(source.read_bytes()) != expected):
            raise ValueError('candidate component or import closure changed')
    if loader is None:
        spec = importlib.util.spec_from_file_location('oxide_tape_candidate_verifier', path)
        if spec is None or spec.loader is None:
            raise ValueError('canonical candidate component cannot be loaded')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
    else:
        module = loader(path)
    provider = module.provider
    before, after = docs['live_before'], docs['live_after']
    if not isinstance(before, list) or not before or before != after:
        raise ValueError('candidate live inventory is missing, empty or changed')
    qualification = docs['qualification']
    guard_command = qualification['guard_command']
    case_command = qualification['case_command']
    if ('--candidate' not in guard_command
            or not any(Path(part).name == 'test-private-build-validation.py' for part in guard_command)
            or not case_command or case_command[-1] != provider.STATUS_SELECTOR):
        raise ValueError('candidate guard or case command is not bound')
    provider.validate_supervisor_receipt(docs['guard'], guard_command, Path(qualification['guard_scratch']))
    provider.validate_supervisor_receipt(docs['case_guard'], case_command, Path(qualification['case_scratch']))
    copied = Path(candidate['copy']['path'])
    root = copied.parent.parent
    probe = qualification['status_probe_id']
    if not isinstance(probe, str) or not provider.BLOCK_ID.fullmatch(probe):
        raise ValueError('candidate status sentinel identity invalid')
    provider.verify_probe(docs['case_log'], home=root / 'home', config=root / 'config.toml',
                          pool=root / 'scratch/pool', registry_probe=root / ('registry-probe-' + probe),
                          cache_probe=root / 'home/Library/Caches/oxide/build-pool')
    remaining = module.verify_candidate_facts(docs['gate'], candidate, qualification, digests,
                                               context['barge_head'], context['barge_tree'])
    issuance = docs['issuance']
    if (issuance.get('schema') != module.ISSUANCE_SCHEMA
            or issuance.get('qualification_sha256') != digests['qualification']
            or issuance.get('product_head') != context['barge_head']
            or issuance.get('product_tree') != context['barge_tree']
            or issuance.get('candidate_sha256') != candidate['copy']['sha256']
            or issuance.get('consumer_sha256') != COMPONENT_SHA256
            or issuance.get('provider_sha256') != PROVIDER_SHA256
            or block.get('scratch_cap_bytes') != CAP_BYTES
            or issuance.get('pool_cap_bytes') != block['pool_cap_bytes']
            or issuance.get('selectors') != block['selectors']
            or type(issuance.get('max_seconds')) is not int
            or not 0 < issuance['max_seconds'] <= remaining
            or issuance['max_seconds'] > block['max_seconds']
            or not isinstance(issuance.get('planner_reference'), str)
            or not issuance['planner_reference'].strip()
            or not isinstance(issuance.get('one_use_id'), str)
            or not HEX64.fullmatch(issuance['one_use_id'])):
        raise ValueError('candidate issuance does not bind the Barge block')
    for key in ('retained_roots', 'barge_retained_roots', 'oxide_retained_roots',
                'offline_cache_source', 'admission_env', 'admission_audit'):
        if key not in qualification or issuance.get(key) != qualification[key]:
            raise ValueError('candidate issuance differs from qualification')
    retained = [Path(value) for value in issuance['retained_roots']]
    origin = candidate['origin']
    options = {'candidate_lock_sha256': origin['lock_sha256'],
               'candidate_toolchain_sha256': origin['toolchain_sha256'],
               'candidate_receipt': origin['guard']['receipt_path'],
               'expect_candidate_receipt': origin['guard']['receipt_sha256'],
               'candidate_log': origin['guard']['log_path'],
               'expect_candidate_log': origin['guard']['log_sha256'],
               'expect_candidate': issuance['candidate_sha256'],
               'candidate_bytes': origin['artifact']['bytes']}
    checked = provider.validate_candidate_origin(origin_root=Path(origin['root']), options=options,
        ledger_roots=retained, pinned_lock_sha256=candidate['binding']['lock_sha256'],
        pinned_toolchain_sha256=candidate['binding']['toolchain_sha256'])
    if checked != origin or not any(copied == root or copied.is_relative_to(root) for root in retained):
        raise ValueError('candidate origin or retained copy is unbound')
    evidence, run_id, deadline_ns, _ = provider.supervisor_context()
    if (type(issuance.get('expires_ns')) is not int or provider.now_ns() >= issuance['expires_ns']
            or issuance['expires_ns'] > deadline_ns):
        raise ValueError('candidate issuance expired or exceeds supervisor')
    provider.claim_issued_token(Path(issuance_path), digests['issuance'], run_id)
    return min(remaining, issuance['max_seconds']), module


def offline_seeder(path):
    """Load the same reviewed offline seeder, never a network fallback."""
    path = Path(path)
    if (not path.is_absolute() or path.is_symlink() or path.resolve(strict=True) != path
            or not path.is_file() or digest(path.read_bytes()) != SEEDER_SHA256):
        raise ValueError('shared offline seeder missing or changed')
    spec = importlib.util.spec_from_file_location('oxide_offline_seed', path)
    if spec is None or spec.loader is None:
        raise ValueError('shared offline seeder unavailable')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.seed_cargo_home


def validate_block(block, *, head, wrapper_hash, oxide_hash, audit_hash, ledger_hash):
    """A block is not manufactured here; its exact digest is supplied externally."""
    required = {'schema', 'task', 'block_id', 'authorization_reference', 'head', 'barge_tree',
                'wrapper_sha256', 'oxide_sha256', 'audit_sha256', 'ledger_sha256',
                'max_seconds', 'scratch_cap_bytes', 'pool_cap_bytes', 'selectors'}
    if set(block) != required or block.get('schema') != 'barge.validation-block/1':
        raise ValueError('incomplete validation block')
    if block['task'] != TASK or block['head'] != head:
        raise ValueError('block task or head mismatch')
    if not isinstance(block['barge_tree'], str) or not re.fullmatch('[0-9a-f]{40}', block['barge_tree']):
        raise ValueError('exact Barge tree is required')
    if not isinstance(block['block_id'], str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', block['block_id']):
        raise ValueError('invalid block identity')
    if not isinstance(block['authorization_reference'], str) or not block['authorization_reference'].strip():
        raise ValueError('operator authorization reference required')
    for key, actual in (('wrapper_sha256', wrapper_hash), ('oxide_sha256', oxide_hash),
                        ('audit_sha256', audit_hash), ('ledger_sha256', ledger_hash)):
        if not isinstance(actual, str) or not HEX64.fullmatch(actual) or block[key] != actual:
            raise ValueError(f'{key} mismatch')
    if type(block['max_seconds']) is not int or block['max_seconds'] < 60:
        raise ValueError('explicit bounded seconds required')
    if block['scratch_cap_bytes'] != CAP_BYTES:
        raise ValueError('private scratch cap must be the approved 3 GiB')
    if type(block['pool_cap_bytes']) is not int or not 0 < block['pool_cap_bytes'] <= CAP_BYTES:
        raise ValueError('private pool budget is missing or exceeds scratch cap')
    selectors = block['selectors']
    if (not isinstance(selectors, list) or not selectors
            or any(not isinstance(selector, str) for selector in selectors)
            or selectors[0] != 'rust:test:tape-private-status'
            or len(set(selectors)) != len(selectors)
            or any(selector not in SAFE_SELECTORS for selector in selectors[1:])):
        raise ValueError('block selectors are missing, duplicated or outside the safe route')
    if 'rust:test:tape-stage' in selectors and 'rust:test:tape-export' in selectors:
        raise ValueError('export already invokes the stage selector')
    return block['max_seconds']


def validate_audit(audit, *, private, effigy_hash, oxide_hash, wrapper_hash, git_hash, rustc_hash, supervisor_hash):
    """Require source-backed evidence for every subprocess boundary, not a flag."""
    if audit.get('schema') != 'barge.validation-source-audit/1':
        raise ValueError('source audit missing')
    if not isinstance(audit.get('review_reference'), str) or not audit['review_reference'].strip():
        raise ValueError('independent source audit review reference required')
    if audit.get('oxide_commit') != OXIDE_COMMIT or audit.get('oxide_tree') != OXIDE_TREE:
        raise ValueError('Oxide source identity mismatch')
    for key, expected in (('effigy_sha256', effigy_hash), ('oxide_sha256', oxide_hash),
                          ('wrapper_sha256', wrapper_hash), ('git_sha256', git_hash),
                          ('rustc_sha256', rustc_hash), ('supervisor_sha256', supervisor_hash)):
        if audit.get(key) != expected or not HEX64.fullmatch(expected):
            raise ValueError(f'audit {key} mismatch')
    components = audit.get('components')
    if not isinstance(components, dict) or set(components) != REQUIRED_AUDIT:
        raise ValueError('transitive audit components incomplete')
    for name, component in components.items():
        if not isinstance(component, dict) or set(component) != {'sources', 'conclusion', 'analysis'}:
            raise ValueError(f'{name}: malformed audit')
        if component['conclusion'] != 'private-destinations-proven':
            raise ValueError(f'{name}: private destination unresolved')
        if not isinstance(component['analysis'], str) or len(component['analysis'].strip()) < 40:
            raise ValueError(f'{name}: source analysis missing')
        sources = component['sources']
        if not isinstance(sources, list) or not sources:
            raise ValueError(f'{name}: source evidence missing')
        for item in sources:
            if not isinstance(item, dict) or set(item) != {'path', 'sha256', 'lines'}:
                raise ValueError(f'{name}: invalid source binding')
            path = Path(item['path'])
            expected = item['sha256']
            if (not path.is_absolute() or not path.is_relative_to(private)
                    or not isinstance(expected, str) or not HEX64.fullmatch(expected)
                    or not isinstance(item['lines'], str) or not item['lines'].strip()):
                raise ValueError(f'{name}: invalid source identity')
            if (path.is_symlink() or path.resolve(strict=True) != path
                    or not path.is_file() or digest(path.read_bytes()) != expected):
                raise ValueError(f'{name}: source changed or unreadable')


def validate_ledger(ledger, private_root):
    """Require a separately inventoried list, including all known evidence."""
    if ledger.get('schema') != 'barge.private-roots/1' or ledger.get('task') != TASK:
        raise ValueError('task private-root ledger missing')
    if not isinstance(ledger.get('review_reference'), str) or not ledger['review_reference'].strip():
        raise ValueError('retained-root discovery review reference required')
    evidence = ledger.get('discovery_evidence')
    if not isinstance(evidence, list) or not evidence:
        raise ValueError('retained-root discovery evidence missing')
    for item in evidence:
        if not isinstance(item, dict) or set(item) != {'path', 'sha256'}:
            raise ValueError('malformed root-discovery evidence')
        path = Path(item['path'])
        if (not path.is_absolute() or not path.is_relative_to(SCRATCH) or path.is_symlink()
                or path.resolve(strict=True) != path or not path.is_file()
                or not isinstance(item['sha256'], str) or not HEX64.fullmatch(item['sha256'])
                or digest(path.read_bytes()) != item['sha256']):
            raise ValueError('root-discovery evidence is missing or changed')
    roots = ledger.get('roots')
    if not isinstance(roots, list) or not roots or any(not isinstance(p, str) for p in roots):
        raise ValueError('private-root ledger is empty or malformed')
    paths = [Path(p) for p in roots]
    if any(not p.is_absolute() or p == SCRATCH or not p.is_relative_to(SCRATCH) for p in paths):
        raise ValueError('private-root ledger escapes scratch')
    required = {SCRATCH / 'b041-final.n0hH3Cez', SCRATCH / '041-budget-ledger-20261102', private_root}
    if not required.issubset(paths) or len(set(paths)) != len(paths):
        raise ValueError('known retained roots or new root missing')
    historic = SCRATCH / '041-budget-ledger-20261102/ledger.json'
    with historic.open('r', encoding='utf-8') as source:
        history = json.load(source)
    if history.get('taskId') != TASK:
        raise ValueError('historical ledger task mismatch')
    known = [Path(item['path']) for item in history.get('known_inner_runtime_logs', [])]
    if not known:
        raise ValueError('historical retained log list unavailable')
    known.extend(SCRATCH.glob('041-pr.*'))
    if any(not any(path == root or root in path.parents for root in paths) for path in known):
        raise ValueError('identified task-owned evidence omitted from root ledger')
    if any(not any(Path(item['path']) == root or root in Path(item['path']).parents for root in paths)
           for item in evidence):
        raise ValueError('root-discovery evidence is not counted')
    for i, left in enumerate(paths):
        if any(left in right.parents or right in left.parents for right in paths[i + 1:]):
            raise ValueError('overlapping private roots are ambiguous')
    return paths


def allocated_bytes(paths):
    """Count allocated blocks without following links; missing is unknown."""
    seen = set()
    total = 0
    entries = 0
    def charge(st):
        nonlocal total, entries
        entries += 1
        if entries > 500000:
            raise ValueError('private-root inventory entry limit')
        key = (st.st_dev, st.st_ino)
        if key not in seen:
            seen.add(key)
            total += st.st_blocks * 512
        if total > CAP_BYTES:
            raise ValueError('private scratch exceeds 3 GiB')
        if not (stat.S_ISDIR(st.st_mode) or stat.S_ISREG(st.st_mode) or stat.S_ISLNK(st.st_mode)):
            raise ValueError('unsupported private-root entry')
    def walk(fd):
        before = os.fstat(fd)
        with os.scandir(fd) as children:
            names = [child.name for child in children]
        for name in names:
            st = os.stat(name, dir_fd=fd, follow_symlinks=False)
            charge(st)
            if stat.S_ISDIR(st.st_mode):
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                try:
                    opened = os.fstat(child)
                    if (st.st_dev, st.st_ino) != (opened.st_dev, opened.st_ino):
                        raise ValueError('private directory changed while counted')
                    walk(child)
                finally:
                    os.close(child)
            after = os.stat(name, dir_fd=fd, follow_symlinks=False)
            if (st.st_dev, st.st_ino, st.st_mode, st.st_size, st.st_mtime_ns) != (
                    after.st_dev, after.st_ino, after.st_mode, after.st_size, after.st_mtime_ns):
                raise ValueError('private entry changed while counted')
        after = os.fstat(fd)
        if (before.st_dev, before.st_ino, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_mtime_ns):
            raise ValueError('private directory changed while counted')
    for path in paths:
        if path.parent.resolve(strict=True) != path.parent:
            raise ValueError('private-root parent is not canonical')
        st = os.lstat(path)
        charge(st)
        if stat.S_ISDIR(st.st_mode):
            fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                opened = os.fstat(fd)
                if (st.st_dev, st.st_ino) != (opened.st_dev, opened.st_ino):
                    raise ValueError('private root changed while counted')
                walk(fd)
            finally:
                os.close(fd)
    return total


def private_directory(root, relative):
    """Create directory components with no symlink traversal."""
    if relative.is_absolute() or any(part in ('', '.', '..') for part in relative.parts):
        raise ValueError('invalid private directory')
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for component in relative.parts:
            try:
                os.mkdir(component, 0o700, dir_fd=fd)
            except FileExistsError:
                pass
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
            info = os.fstat(fd)
            if info.st_uid != os.geteuid() or info.st_mode & 0o077:
                raise ValueError('private directory is not owner-only')
    finally:
        os.close(fd)


def private_create(root, relative, data):
    """Write one new file without following a parent or final symlink."""
    private_directory(root, relative.parent)
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for component in relative.parts[:-1]:
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
        output = os.open(relative.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
        with os.fdopen(output, 'wb') as file:
            file.write(data)
    finally:
        os.close(fd)


def verify_git_tree(repo, root, commit, expected_tree, git, run):
    """Compare every archived file and mode to the immutable Git tree."""
    env = {'PATH': '/usr/bin:/bin', 'HOME': '/nonexistent', 'GIT_CONFIG_NOSYSTEM': '1',
           'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_OPTIONAL_LOCKS': '0'}
    def git_output(*args):
        return run([str(git), '-C', str(repo), *args], env)
    tree = git_output('rev-parse', commit + '^{tree}').decode('ascii').strip()
    if tree != expected_tree or git_output('rev-parse', commit + '^{commit}').decode('ascii').strip() != commit:
        raise ValueError('source commit or tree mismatch')
    records = git_output('ls-tree', '-rz', '--full-tree', commit).split(b'\0')
    expected = {}
    for record in records:
        if not record:
            continue
        header, raw = record.split(b'\t', 1)
        mode, kind, blob = header.decode('ascii').split(' ')
        name = raw.decode('utf-8')
        if (kind != 'blob' or mode not in ('100644', '100755', '120000')
                or not re.fullmatch('[0-9a-f]{40}', blob) or name in expected):
            raise ValueError('unsupported source tree member')
        relative = Path(name)
        if relative.is_absolute() or any(part in ('', '.', '..') for part in relative.parts):
            raise ValueError('invalid source tree path')
        path = root / relative
        current = root
        for component in relative.parts[:-1]:
            current = current / component
            if current.is_symlink() or not current.is_dir():
                raise ValueError('source tree parent is not a real directory')
        if mode == '120000':
            if not path.is_symlink():
                raise ValueError('source symlink mismatch')
            content = os.fsencode(os.readlink(path))
        else:
            if path.is_symlink() or not path.is_file():
                raise ValueError('source file mismatch')
            if bool(path.stat().st_mode & 0o111) != (mode == '100755'):
                raise ValueError('source executable mode mismatch')
            content = path.read_bytes()
        if hashlib.sha1(b'blob ' + str(len(content)).encode('ascii') + b'\0' + content).hexdigest() != blob:
            raise ValueError('source bytes differ from Git tree')
        expected[name] = digest(content)
    actual = {path.relative_to(root).as_posix() for path in root.rglob('*')
              if path.is_file() or path.is_symlink()}
    if actual != set(expected):
        raise ValueError('source archive membership mismatch')
    expected_dirs = {parent.as_posix() for name in expected for parent in Path(name).parents if parent != Path('.')}
    actual_dirs = {path.relative_to(root).as_posix() for path in root.rglob('*')
                   if path.is_dir() and not path.is_symlink()}
    if actual_dirs != expected_dirs:
        raise ValueError('source archive directory mismatch')
    return expected


def source_snapshot(root):
    """Detect a changed archive during a supervised child without following links."""
    result = {}
    for path in root.rglob('*'):
        name = path.relative_to(root).as_posix()
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            result[name] = ('link', os.readlink(path))
        elif stat.S_ISREG(info.st_mode):
            result[name] = ('file', bool(info.st_mode & 0o111), digest(path.read_bytes()))
        elif stat.S_ISDIR(info.st_mode):
            result[name] = ('directory',)
        else:
            raise ValueError('unsupported source archive member')
    return result


def verify_probe(stdout, *, home, config, pool, registry_probe, cache_probe):
    """Check the real recorded-status output, including both unique sentinels."""
    lines = stdout.splitlines()
    required = (
        f'config: {config} (selected by OXIDE_CONFIG)',
        f'  home: HOME {home} (proof control OXIDE_TEST_HOME_FROM_ENV)',
        f'other pool path: {registry_probe} (unrecognised; left alone)',
        f'other pool path: {cache_probe} (unrecognised; left alone)',
    )
    if any(lines.count(line) != 1 for line in required):
        raise ValueError('effective config, home, state or cache sentinel mismatch')
    pools = [line for line in lines if line.startswith('pool: ')]
    if len(pools) != 1 or not pools[0].startswith(f'pool: {pool} (') or 'unavailable' in pools[0]:
        raise ValueError('effective private pool was not positively resolved')
    if any('cannot open:' in line for line in lines):
        raise ValueError('status refused the private pool')


def verify_inventory(before, after):
    if before != after:
        raise ValueError('live inventory changed; isolation failed')


def host_admission(inherited, audit):
    """Preserve only the independently audited host admission route."""
    hook = inherited.get('EFFIGY_ADMISSION_HOOK')
    socket = inherited.get('QUEUE_ADMISSION_SOCKET')
    if (not isinstance(hook, str) or not hook.strip()
            or not isinstance(socket, str) or not Path(socket).is_absolute()
            or hook != audit.get('admission_hook') or socket != audit.get('admission_socket')):
        raise ValueError('audited host admission hook and socket required')
    for name in ('admission_executables', 'admission_runtime_files'):
        files = audit.get(name)
        if not isinstance(files, list) or not files:
            raise ValueError(f'{name} bindings missing')
        for item in files:
            if not isinstance(item, dict) or set(item) != {'path', 'sha256'}:
                raise ValueError(f'invalid {name} binding')
            path = Path(item['path'])
            if (not path.is_absolute() or path.is_symlink() or path.resolve(strict=True) != path
                    or not path.is_file() or not isinstance(item['sha256'], str)
                    or not HEX64.fullmatch(item['sha256']) or digest(path.read_bytes()) != item['sha256']):
                raise ValueError(f'{name} is missing or changed')
    return {'EFFIGY_ADMISSION_HOOK': hook, 'QUEUE_ADMISSION_SOCKET': socket}


def direct_effigy(path):
    """Bind the executable itself instead of an unbound script target."""
    resolved = path.resolve(strict=True)
    if not resolved.is_file() or not os.access(resolved, os.X_OK) or resolved.read_bytes().startswith(b'#!'):
        raise ValueError('direct audited Effigy executable required, not a script launcher')
    return resolved


def child_environment(inherited, *, private, oxide, rustc, wrapper_hash, pool_cap, admission_env):
    """Construct an allowlisted environment; never inherit OXIDE/XDG controls."""
    if inherited.get('RUSTUP_TOOLCHAIN') != '1.97.1' or not inherited.get('RUSTUP_HOME'):
        raise ValueError('declared Rust toolchain binding missing')
    env = {key: inherited[key] for key in ('RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'PATH') if key in inherited}
    if set(admission_env) != {'EFFIGY_ADMISSION_HOOK', 'QUEUE_ADMISSION_SOCKET'}:
        raise ValueError('host admission environment missing')
    env.update(admission_env)
    env.update({'HOME': str(private / 'home'), 'CARGO_HOME': str(private / 'cache'),
                'TMPDIR': str(private / 'tmp'), 'TMP': str(private / 'tmp'),
                'TEMP': str(private / 'tmp'), 'OXIDE_CONFIG': str(private / 'config.toml'),
                'OXIDE_POOL': str(private / 'scratch/pool'), 'OXIDE_TASK_SCRATCH_ROOT': str(private),
                'OXIDE_SCRATCH_ROOT': str(private / 'scratch'),
                'OXIDE_POOL_BACKEND': 'directory-budget', 'OXIDE_POOL_CAP': str(pool_cap),
                'OXIDE_TEST_HOME_FROM_ENV': '1', 'BARGE_PRIVATE_ROOT': str(private),
                'BARGE_PRIVATE_OXIDE': str(oxide), 'BARGE_PINNED_RUSTC': str(rustc),
                'RUSTC': str(rustc),
                'BARGE_PRIVATE_WRAPPER_SHA256': wrapper_hash,
                'CARGO_NET_OFFLINE': 'true', 'RUSTUP_AUTO_INSTALL': '0',
                'PATH': str(private / 'bin') + os.pathsep + env.get('PATH', '')})
    return env


def verify_supervisor(receipt, expected_command, private):
    outcome = receipt.get('outcome', {})
    census = receipt.get('census', {})
    if (receipt.get('command') != expected_command or receipt.get('scratch') != str(private)
            or outcome.get('closure') != 'closed' or outcome.get('status') != 0
            or outcome.get('fault') is not None or outcome.get('deadline') is not False
            or outcome.get('launched') is not True or census.get('certain') is not True
            or census.get('members') != 0 or census.get('uncertain') != 0):
        raise ValueError('supervisor receipt does not prove successful closure')
