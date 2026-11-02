#!/usr/bin/env python3
"""Effect-free refusal controls. No process, inventory, build or fixture."""
import copy
import unittest
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
                                          wrapper_hash=self.hash, pool_cap=4096)
        self.assertEqual(env['OXIDE_POOL'], str(private / 'scratch/pool'))
        self.assertEqual(env['OXIDE_SCRATCH_ROOT'], str(private / 'scratch'))
        self.assertEqual(env['OXIDE_POOL_BACKEND'], 'directory-budget')
        for key in ('XDG_STATE_HOME', 'BARGE_STATE_DIR', 'RUSTFLAGS', 'DYLD_INSERT_LIBRARIES'):
            self.assertNotIn(key, env)

    def test_same_content_replaced_inode_still_fails(self):
        before = [('./state/pools.json', (1, 11, 0o100600, 4, 7), 'same-digest')]
        after = [('./state/pools.json', (1, 12, 0o100600, 4, 7), 'same-digest')]
        with self.assertRaises(ValueError):
            admission.verify_inventory(before, after)
        admission.verify_inventory(before, before)

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
