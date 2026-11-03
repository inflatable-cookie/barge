#!/usr/bin/env python3
"""Effect-free refusal controls. No process, inventory, build or fixture."""
import copy
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

    def test_candidate_delegates_and_refuses_unqualified_evidence(self):
        candidate = {'schema': admission.CANDIDATE_SCHEMA, 'mode': 'candidate',
                     'qualification': 'unqualified', 'build': {'state': 'not-built'}}
        context = {'barge_head': 'b' * 40, 'barge_tree': 'c' * 40,
                   'wrapper_sha256': self.hash, 'component_sha256': self.hash,
                   'block_sha256': self.hash, 'private_root': '/private'}
        result = {'schema': 'oxide.capture.candidate-verification/1', **context,
                  'candidate_sha256': self.hash}
        flags = ('qualified', 'case_and_guard_passed', 'claim_consumed', 'offline_dependencies_bound',
                 'origin_copy_source_bound', 'toolchain_tools_bound', 'fresh_live_inventory_equal',
                 'remaining_budget_bound', 'component_source_bound')
        result.update({key: True for key in flags})
        verifier = mock.Mock(return_value=result)
        module = mock.Mock(verify_candidate=verifier)
        kwargs = dict(candidate_hash=self.hash, component_path='/private/verified.py', component_hash=self.hash,
                      qualification={'case': 'passed'}, guard={'closure': 'closed'},
                      issuance={'one_use': 'fresh'}, context=context, loader=lambda _: module)
        with mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission.Path, 'is_symlink', return_value=False), \
                mock.patch.object(admission.Path, 'is_file', return_value=True), \
                mock.patch.object(admission.Path, 'read_bytes', return_value=b'component'), \
                mock.patch.object(admission, 'digest', return_value=self.hash):
            self.assertEqual(admission.candidate_runtime(candidate, **kwargs), result)
            verifier.assert_called_once_with(candidate=candidate, qualification=kwargs['qualification'],
                                            guard=kwargs['guard'], issuance=kwargs['issuance'], context=context)
            for key in flags:
                verifier.return_value = {**result, key: False}
                with self.assertRaises(ValueError):
                    admission.candidate_runtime(candidate, **kwargs)
            for change in ({'barge_head': 'd' * 40}, {'candidate_sha256': 'b' * 64}):
                verifier.return_value = {**result, **change}
                with self.assertRaises(ValueError):
                    admission.candidate_runtime(candidate, **kwargs)
            verifier.return_value = result
            with self.assertRaises(ValueError):
                admission.candidate_runtime({**candidate, 'qualification': 'qualified'}, **kwargs)
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'component_hash': 'b' * 64})
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'guard': {}})
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'qualification': {}})
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'issuance': {}})
            with self.assertRaises(ValueError):
                admission.candidate_runtime(candidate, **{**kwargs, 'loader': lambda _: object()})

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

    def test_candidate_budget_and_component_must_match_issuance(self):
        result = {'scratch_cap_bytes': admission.CAP_BYTES, 'pool_cap_bytes': 512,
                  'remaining_seconds': 100}
        block = {'scratch_cap_bytes': admission.CAP_BYTES, 'pool_cap_bytes': 512}
        audit = {'candidate_component_sha256': self.hash, 'candidate_component_commit': 'c' * 40}
        self.assertEqual(admission.candidate_limits(result, block, 120, audit, self.hash, 'c' * 40), 100)
        for changes in ({'remaining_seconds': 0}, {'remaining_seconds': 121},
                        {'remaining_seconds': None}, {'scratch_cap_bytes': admission.CAP_BYTES + 1},
                        {'pool_cap_bytes': 513}):
            with self.assertRaises(ValueError):
                admission.candidate_limits({**result, **changes}, block, 120, audit, self.hash, 'c' * 40)
        with self.assertRaises(ValueError):
            admission.candidate_limits(result, block, 120, audit, 'b' * 64, 'c' * 40)

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
