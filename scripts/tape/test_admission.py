#!/usr/bin/env python3
"""Effect-free refusal controls. No process, inventory, build or fixture."""
import copy
import importlib.util
import os
import unittest
from unittest import mock
import admission


class AdmissionControls(unittest.TestCase):
    def setUp(self):
        self.hash = 'a' * 64
        self.block = {
            'schema': 'barge.validation-block/1', 'task': admission.TASK,
            'block_id': 'one', 'authorization_reference': 'operator-decision',
            'head': 'b' * 40, 'barge_tree': 'c' * 40, 'wrapper_sha256': self.hash, 'oxide_sha256': self.hash,
            'audit_sha256': self.hash, 'ledger_sha256': self.hash, 'max_seconds': 600,
            'scratch_cap_bytes': admission.CAP_BYTES, 'pool_cap_bytes': 512 * 1024 * 1024,
            'selectors': ['rust:test:tape-private-status', 'rust:compile'],
        }
        self.bindings = dict(head='b' * 40, wrapper_hash=self.hash, oxide_hash=self.hash,
                             audit_hash=self.hash, ledger_hash=self.hash)

    def test_source_snapshot_allows_only_new_guarded_target(self):
        root = admission.Path('/private/work')
        private = admission.Path('/private')
        members = [root / 'Cargo.lock', root / 'source.rs', root / 'link']
        modes = {'Cargo.lock': 0o100644, 'source.rs': 0o100644, 'link': 0o120777,
                 'target': 0o040700, 'unrelated': 0o100644}
        contents = {'Cargo.lock': b'lock', 'source.rs': b'source', 'unrelated': b'new'}
        link = ['original']
        def metadata(path):
            return mock.Mock(st_mode=modes[path.name])
        with mock.patch.object(admission.Path, 'iterdir', lambda _: iter(members)), \
                mock.patch.object(admission.Path, 'rglob', lambda self, pattern: iter(())), \
                mock.patch.object(admission.Path, 'lstat', metadata), \
                mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission.Path, 'is_symlink', lambda self: self.name == 'link'), \
                mock.patch.object(admission.Path, 'is_dir', lambda self: self.name == 'target'), \
                mock.patch.object(admission.Path, 'read_bytes', lambda self: contents[self.name]), \
                mock.patch.object(admission.os, 'readlink', lambda _: link[0]), \
                mock.patch.object(admission, 'validate_generated_output') as validate:
            baseline = admission.source_snapshot(root)
            members.append(root / 'target')
            self.assertEqual(admission.source_snapshot(root, private=private, baseline=baseline), baseline)
            validate.assert_called_once_with(root / 'target', private)
            self.assertNotEqual(admission.source_snapshot(root), baseline)  # Oxide stays strict.
            contents['Cargo.lock'] = b'changed'
            self.assertNotEqual(admission.source_snapshot(root, private=private, baseline=baseline), baseline)
            contents['Cargo.lock'] = b'lock'
            modes['source.rs'] = 0o100755
            self.assertNotEqual(admission.source_snapshot(root, private=private, baseline=baseline), baseline)
            modes['source.rs'] = 0o100644
            link[0] = 'changed'
            self.assertNotEqual(admission.source_snapshot(root, private=private, baseline=baseline), baseline)
            link[0] = 'original'
            members.append(root / 'unrelated')
            self.assertNotEqual(admission.source_snapshot(root, private=private, baseline=baseline), baseline)

    def test_generated_link_escape_is_rejected_before_external_stat(self):
        private = admission.Path('/private')
        info = mock.Mock(st_uid=501, st_mode=0o120777)
        with mock.patch.object(admission.os, 'open', return_value=10), \
                mock.patch.object(admission.os, 'close'), \
                mock.patch.object(admission.os, 'geteuid', return_value=501), \
                mock.patch.object(admission.os, 'fstat', return_value=mock.Mock(st_mode=0o040700)), \
                mock.patch.object(admission.os, 'stat', return_value=info) as inspect, \
                mock.patch.object(admission.os, 'readlink', return_value='/outside/payload'):
            with self.assertRaisesRegex(ValueError, 'escapes'):
                admission._owned_destination(private / 'target', private)
            inspect.assert_called_once_with('target', dir_fd=10, follow_symlinks=False)

    def test_generated_target_checks_owned_output_entries_without_reading_them(self):
        private = admission.Path('/private')
        target = private / 'work/target'
        directory = mock.Mock(st_uid=501, st_mode=0o040700, st_dev=1, st_ino=2)
        output = mock.Mock(st_uid=501, st_mode=0o100600, st_dev=1, st_ino=3)
        child = mock.Mock()
        child.name = 'artifact'
        scan = mock.MagicMock()
        scan.__enter__.return_value = iter([child])
        with mock.patch.object(admission.os, 'lstat', return_value=directory), \
                mock.patch.object(admission.os, 'geteuid', return_value=501), \
                mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission, '_owned_destination',
                                  side_effect=[(target, directory, 10), (target / 'artifact', output, None)]), \
                mock.patch.object(admission.os, 'scandir', return_value=scan), \
                mock.patch.object(admission.os, 'close'), \
                mock.patch.object(admission.Path, 'read_bytes', side_effect=AssertionError('output was read')):
            admission.validate_generated_output(target, private)

    def test_actual_candidate_component_composition(self):
        path = admission.Path(os.environ['BARGE_TEST_CANDIDATE_COMPONENT'])
        spec = importlib.util.spec_from_file_location('actual_candidate_test', path)
        component = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(component)
        provider = component.provider
        hashes = {name: str(index) * 64 for index, name in enumerate(
            ('gate', 'candidate', 'qualification', 'issuance', 'guard', 'case_guard', 'live_before'), 1)}
        hashes['live_after'] = hashes['live_before']
        hashes['case_log'] = '9' * 64
        artifact = 'a' * 64
        origin = {'root': '/old', 'artifact': {'sha256': artifact, 'bytes': 10},
                  'lock_sha256': 'lock', 'toolchain_sha256': 'toolchain',
                  'guard': {'closure': 'closed', 'status': 0, 'census_certain': True,
                            'census_members': 0, 'census_uncertain': 0,
                            'receipt_sha256': 'origin-guard', 'log_sha256': 'origin-log',
                            'receipt_path': '/origin-receipt', 'log_path': '/origin-log'}}
        candidate = {'schema': provider.SCHEMA_CANDIDATE_RECEIPT, 'mode': 'candidate',
                     'qualification': 'unqualified', 'ruling': provider.CANDIDATE_RULING,
                     'status_phase': provider.STATUS_SELECTOR, 'build': {'state': 'not-built'},
                     'origin': origin, 'copy': {'path': '/old/candidate/oxide', 'sha256': artifact},
                     'binding': {'commit': provider.OXIDE_PIN_COMMIT, 'lock_sha256': 'lock',
                                 'toolchain_sha256': 'toolchain', 'oxide_sha256': artifact},
                     'live_inventory': {'before_sha256': hashes['live_before'], 'after_sha256': hashes['live_after']}}
        guard_command = ['python', 'test-private-build-validation.py', '--candidate']
        case_command = ['effigy', provider.STATUS_SELECTOR]
        allowance = {'remaining_seconds': 1000}
        gate = {'schema': provider.SCHEMA_GATE_RECEIPT, 'qualification': {'state': 'unqualified'},
                'failures': [], 'blocks': {'oxide_block_sha256': 'prior-block'}, 'allowance': allowance,
                'commands': [{'phase': provider.STATUS_SELECTOR, 'exit': 0, 'argv': ['guard', '--', *case_command]}],
                'exports': {'candidate_receipt_sha256': hashes['candidate'],
                            'candidate_copy_path': '/old/candidate/oxide', 'candidate_copy_sha256': artifact,
                            'candidate_origin_root': '/old', 'origin_guard_receipt_sha256': 'origin-guard',
                            'origin_build_log_sha256': 'origin-log', 'live_before_sha256': hashes['live_before'],
                            'live_after_sha256': hashes['live_after'], 'remaining_allowance': allowance}}
        def guard(command):
            return {'command': command, 'scratch': '/old',
                    'outcome': {'closure': 'closed', 'status': 0, 'fault': None, 'deadline': False, 'launched': True},
                    'census': {'certain': True, 'members': 0, 'uncertain': 0}}
        shared = {'retained_roots': ['/old'], 'barge_retained_roots': ['/old'], 'oxide_retained_roots': [],
                  'offline_cache_source': '/old/cache', 'admission_env': {'hook': 'audited'},
                  'admission_audit': {'source': 'audited'}}
        qual = {'schema': component.QUALIFICATION_SCHEMA, 'state': 'qualified', 'review_reference': 'review',
                'gate_sha256': hashes['gate'], 'candidate_sha256': hashes['candidate'],
                'guard_sha256': hashes['guard'], 'case_guard_sha256': hashes['case_guard'],
                'case_log_sha256': hashes['case_log'], 'guard_command': guard_command, 'guard_scratch': '/old',
                'case_command': case_command, 'case_scratch': '/old', 'status_probe_id': 'probe',
                'product_head': '1' * 40, 'product_tree': '2' * 40, 'producer_oxide_block_sha256': 'prior-block',
                'live_before_sha256': hashes['live_before'], 'live_after_sha256': hashes['live_after'], **shared}
        issuance = {'schema': component.ISSUANCE_SCHEMA, 'qualification_sha256': hashes['qualification'],
                    'product_head': '1' * 40, 'product_tree': '2' * 40, 'candidate_sha256': artifact,
                    'consumer_sha256': admission.COMPONENT_SHA256, 'provider_sha256': admission.PROVIDER_SHA256,
                    'scratch_cap_bytes': admission.CAP_BYTES, 'pool_cap_bytes': 100,
                    'selectors': ['rust:test:tape-private-status', 'rust:compile'],
                    'max_seconds': 600, 'planner_reference': 'issued', 'one_use_id': 'b' * 64,
                    'expires_ns': 2000, **shared}
        status = ('config: /old/config.toml (selected by OXIDE_CONFIG)\n'
                  '  home: HOME /old/home (proof control OXIDE_TEST_HOME_FROM_ENV)\n'
                  'pool: /old/scratch/pool (from OXIDE_POOL)\n'
                  'other pool path: /old/registry-probe-probe (unrecognised; left alone)\n'
                  'other pool path: /old/home/Library/Caches/oxide/build-pool (unrecognised; left alone)')
        docs = {'gate': gate, 'qualification': qual, 'issuance': issuance, 'guard': guard(guard_command),
                'case_guard': guard(case_command), 'case_log': status,
                'live_before': [['state/pools.json']], 'live_after': [['state/pools.json']]}
        block = {'scratch_cap_bytes': admission.CAP_BYTES, 'pool_cap_bytes': 100, 'max_seconds': 700,
                 'selectors': ['rust:test:tape-private-status', 'rust:compile']}
        context = {'barge_head': '1' * 40, 'barge_tree': '2' * 40}
        kwargs = dict(component_path=path, component_hash=admission.COMPONENT_SHA256,
                      component_commit=admission.COMPONENT_COMMIT, docs=docs, digests=hashes,
                      context=context, issuance_path='/issued', block=block, loader=lambda _: component)
        with mock.patch.object(provider, 'validate_candidate_origin', return_value=origin), \
                mock.patch.object(provider, 'supervisor_context', return_value=(admission.Path('/ev'), 'run', 1000, admission.Path('/root'))), \
                mock.patch.object(provider, 'now_ns', return_value=1), \
                mock.patch.object(provider, 'claim_issued_token') as claim:
            remaining, actual = admission.candidate_runtime(candidate, **kwargs)
            self.assertEqual((remaining, actual), (600, component))
            self.assertEqual(admission.candidate_runtime_bounds(1000, issuance['expires_ns'], 700, remaining),
                             (1000, 600))
            self.assertEqual(admission.candidate_runtime_bounds(1000, 900, 500, remaining), (900, 500))
            claim.assert_called_once()
            for section, key, value in (('qualification', 'state', 'unqualified'),
                                        ('live_before', None, []), ('guard', None, {}),
                                        ('issuance', 'pool_cap_bytes', 101),
                                        ('issuance', 'expires_ns', 1), ('issuance', 'expires_ns', None),
                                        ('issuance', 'expires_ns', True),
                                        ('issuance', 'expires_ns', '2000'), ('issuance', 'max_seconds', 701)):
                changed = copy.deepcopy(docs)
                if key is None:
                    changed[section] = value
                else:
                    changed[section][key] = value
                with self.subTest(section=section), self.assertRaises((ValueError, KeyError, provider.GateRefusal)):
                    admission.candidate_runtime(candidate, **{**kwargs, 'docs': changed})
            with mock.patch.object(provider, 'now_ns', return_value=1000), self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **kwargs)
            claim.assert_called_once()
            with self.assertRaises((ValueError, provider.GateRefusal)):
                admission.candidate_runtime(candidate, **{**kwargs, 'context': {**context, 'barge_head': '3' * 40}})
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'block': {**block, 'scratch_cap_bytes': admission.CAP_BYTES + 1}})
            claim.side_effect = provider.GateRefusal('replayed')
            with self.assertRaises(provider.GateRefusal):
                admission.candidate_runtime(candidate, **kwargs)
            claim.side_effect = None
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'component_hash': 'b' * 64})
            with mock.patch.object(admission.Path, 'read_bytes', side_effect=lambda: b'changed'):
                with self.assertRaises(ValueError):
                    admission.candidate_runtime(candidate, **kwargs)
            original_read = admission.Path.read_bytes
            def changed_provider(file):
                return b'changed' if file.name == 'test-private-build-validation.py' else original_read(file)
            with mock.patch.object(admission.Path, 'read_bytes', autospec=True, side_effect=changed_provider):
                with self.assertRaises(ValueError):
                    admission.candidate_runtime(candidate, **kwargs)

    def test_block_requires_explicit_exact_scope_and_binding(self):
        self.assertEqual(admission.validate_block(self.block, **self.bindings), 600)
        for change in ({'head': 'c' * 40}, {'authorization_reference': ''},
                       {'scratch_cap_bytes': admission.CAP_BYTES + 1},
                       {'selectors': ['rust:test:tape-private-status', 'rust:test']},
                       {'selectors': ['rust:test:tape-private-status', 'rust:compile', 'rust:compile']},
                       {'selectors': ['rust:test:tape-private-status', 'rust:test:tape-eval']},
                       {'selectors': ['rust:test:tape-private-status', 'rust:test:tape-stage',
                                      'rust:test:tape-export']},
                       {'max_seconds': 0}):
            invalid = {**self.block, **change}
            with self.assertRaises(ValueError):
                admission.validate_block(invalid, **self.bindings)

    def test_approved_scratch_cap_is_exact_and_pool_is_bounded(self):
        cap = 3 * 1024 * 1024 * 1024
        self.assertEqual(admission.CAP_BYTES, cap)
        self.assertEqual(admission.validate_block({**self.block, 'pool_cap_bytes': cap}, **self.bindings), 600)
        for changes in ({'scratch_cap_bytes': 2 * 1024 * 1024 * 1024},
                        {'scratch_cap_bytes': cap + 1}, {'scratch_cap_bytes': None},
                        {'pool_cap_bytes': cap + 1}, {'pool_cap_bytes': None}, {'pool_cap_bytes': 0}):
            with self.assertRaises(ValueError):
                admission.validate_block({**self.block, **changes}, **self.bindings)

    def test_retained_root_allocation_refuses_above_approved_cap(self):
        root = admission.Path('/scratch/retained')
        file_stat = mock.Mock(st_dev=1, st_ino=2, st_mode=0o100600,
                              st_blocks=admission.CAP_BYTES // 512)
        with mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission.os, 'lstat', return_value=file_stat):
            self.assertEqual(admission.allocated_bytes([root]), admission.CAP_BYTES)
            file_stat.st_blocks += 1
            with self.assertRaises(ValueError):
                admission.allocated_bytes([root])

    def test_dry_requires_state_and_cache_sentinels(self):
        args = dict(home='/p/home', config='/p/config', pool='/p/scratch/pool',
                    registry_probe='/p/reg', cache_probe='/p/cache')
        lines = ['config: /p/config (selected by OXIDE_CONFIG)',
                 '  home: HOME /p/home (proof control OXIDE_TEST_HOME_FROM_ENV)',
                 'pool: /p/scratch/pool (from OXIDE_POOL)',
                 'other pool path: /p/reg (unrecognised; left alone)',
                 'other pool path: /p/cache (unrecognised; left alone)']
        admission.verify_probe('\n'.join(lines), **args)
        for index in range(len(lines)):
            with self.assertRaises(ValueError):
                admission.verify_probe('\n'.join(lines[:index] + lines[index + 1:]), **args)

    def test_child_environment_scrubs_inherited_controls(self):
        inherited = {'PATH': '/bin', 'RUSTUP_HOME': '/rustup', 'RUSTUP_TOOLCHAIN': '1.97.1',
                     'OXIDE_POOL': '/live', 'OXIDE_SCRATCH_ROOT': '/live-scratch',
                     'XDG_STATE_HOME': '/live-state', 'BARGE_STATE_DIR': '/live-barge',
                     'RUSTFLAGS': 'unsafe', 'DYLD_INSERT_LIBRARIES': '/injected'}
        private = admission.Path('/Volumes/Builds/scratch/barge/private')
        env = admission.child_environment(inherited, private=private, oxide=private / 'bin/oxide',
                                          rustc=admission.Path('/rustup/rustc'),
                                          wrapper_hash=self.hash, pool_cap=4096,
                                          admission_env={'EFFIGY_ADMISSION_HOOK': 'audited-hook',
                                                         'QUEUE_ADMISSION_SOCKET': '/queue/socket'})
        self.assertEqual(env['OXIDE_POOL'], str(private / 'scratch/pool'))
        self.assertEqual(env['OXIDE_SCRATCH_ROOT'], str(private / 'scratch'))
        self.assertEqual(env['OXIDE_POOL_BACKEND'], 'directory-budget')
        self.assertEqual(env['EFFIGY_ADMISSION_HOOK'], 'audited-hook')
        self.assertEqual(env['QUEUE_ADMISSION_SOCKET'], '/queue/socket')
        for key in ('XDG_STATE_HOME', 'BARGE_STATE_DIR', 'RUSTFLAGS', 'DYLD_INSERT_LIBRARIES'):
            self.assertNotIn(key, env)

    def test_host_admission_refuses_missing_or_changed_hook(self):
        inherited = {'EFFIGY_ADMISSION_HOOK': 'audited-hook', 'QUEUE_ADMISSION_SOCKET': '/queue/socket'}
        audit = {'admission_hook': 'audited-hook', 'admission_socket': '/queue/socket',
                 'admission_executables': [{'path': '/audited/hook', 'sha256': self.hash}],
                 'admission_runtime_files': [{'path': '/audited/loader', 'sha256': self.hash}]}
        with mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission.Path, 'is_symlink', return_value=False), \
                mock.patch.object(admission.Path, 'is_file', return_value=True), \
                mock.patch.object(admission.Path, 'read_bytes', return_value=b'audited'), \
                mock.patch.object(admission, 'digest', return_value=self.hash):
            self.assertEqual(admission.host_admission(inherited, audit), inherited)
            for changes in ({'EFFIGY_ADMISSION_HOOK': ''},
                            {'EFFIGY_ADMISSION_HOOK': 'different'},
                            {'QUEUE_ADMISSION_SOCKET': '/different'}):
                with self.assertRaises(ValueError):
                    admission.host_admission({**inherited, **changes}, audit)
            with self.assertRaises(ValueError):
                admission.host_admission(inherited, {**audit, 'admission_executables': []})
            with self.assertRaises(ValueError):
                admission.host_admission(inherited, {**audit, 'admission_runtime_files': []})
            with mock.patch.object(admission, 'digest', return_value='b' * 64):
                with self.assertRaises(ValueError):
                    admission.host_admission(inherited, audit)

    def test_same_content_replaced_inode_still_fails(self):
        before = [('./state/pools.json', (1, 11, 0o100600, 4, 7), 'same-digest')]
        after = [('./state/pools.json', (1, 12, 0o100600, 4, 7), 'same-digest')]
        with self.assertRaises(ValueError):
            admission.verify_inventory(before, after)
        admission.verify_inventory(before, before)

    def test_script_launcher_cannot_stand_in_for_effigy_binary(self):
        path = admission.Path('/audited/effigy')
        with mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission.Path, 'is_file', return_value=True), \
                mock.patch.object(admission.os, 'access', return_value=True), \
                mock.patch.object(admission.Path, 'read_bytes', return_value=b'#!/bin/sh\nexec other'):
            with self.assertRaises(ValueError):
                admission.direct_effigy(path)

    def test_audit_does_not_accept_boolean_or_missing_source(self):
        audit = {'schema': 'barge.validation-source-audit/1',
                 'review_reference': 'independent-review',
                 'oxide_commit': admission.OXIDE_COMMIT, 'oxide_tree': admission.OXIDE_TREE,
                 'effigy_sha256': self.hash, 'oxide_sha256': self.hash,
                 'wrapper_sha256': self.hash, 'git_sha256': self.hash,
                 'rustc_sha256': self.hash, 'supervisor_sha256': self.hash, 'components': {}}
        with self.assertRaises(ValueError):
            admission.validate_audit(audit, private=admission.Path('/p'), effigy_hash=self.hash, oxide_hash=self.hash,
                                     wrapper_hash=self.hash, git_hash=self.hash,
                                     rustc_hash=self.hash, supervisor_hash=self.hash)
        audit['components'] = {name: {'sources': [], 'conclusion': 'private-destinations-proven',
                                      'analysis': 'Source-bound private destination evidence is required.'}
                               for name in admission.REQUIRED_AUDIT}
        with self.assertRaises(ValueError):
            admission.validate_audit(audit, private=admission.Path('/p'), effigy_hash=self.hash, oxide_hash=self.hash,
                                     wrapper_hash=self.hash, git_hash=self.hash,
                                     rustc_hash=self.hash, supervisor_hash=self.hash)

    def test_supervisor_must_close_and_census_be_certain(self):
        receipt = {'command': ['cmd'], 'scratch': '/p',
                   'outcome': {'closure': 'closed', 'status': 0, 'fault': None,
                               'deadline': False, 'launched': True},
                   'census': {'certain': True, 'members': 0, 'uncertain': 0}}
        admission.verify_supervisor(receipt, ['cmd'], admission.Path('/p'))
        for key, value in [('closure', 'unclosed'), ('fault', 'timeout'), ('deadline', True)]:
            bad = copy.deepcopy(receipt)
            bad['outcome'][key] = value
            with self.assertRaises(ValueError):
                admission.verify_supervisor(bad, ['cmd'], admission.Path('/p'))
        bad = copy.deepcopy(receipt)
        bad['census']['certain'] = False
        with self.assertRaises(ValueError):
            admission.verify_supervisor(bad, ['cmd'], admission.Path('/p'))

    def test_ledger_cannot_omit_retained_roots(self):
        ledger = {'schema': 'barge.private-roots/1', 'task': admission.TASK,
                  'roots': ['/Volumes/Builds/scratch/barge/new']}
        with self.assertRaises(ValueError):
            admission.validate_ledger(ledger, admission.Path(ledger['roots'][0]))

    def test_inventory_rejects_a_changed_digest_and_metadata(self):
        before = [('./pool', (1, 2, 0o100600, 1, 7), 'first')]
        for after in ([('./pool', (1, 2, 0o100600, 1, 7), 'second')],
                      [('./pool', (1, 2, 0o100600, 1, 8), 'first')]):
            with self.assertRaises(ValueError):
                admission.verify_inventory(before, after)


if __name__ == '__main__':
    unittest.main()
