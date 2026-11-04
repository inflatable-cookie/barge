#!/usr/bin/env python3
"""Pure source controls for the held tape export producer."""
import unittest
from unittest import mock
import admission
import export
from export import git_blob


class GitBlobIdentity(unittest.TestCase):
    def test_empty_blob_uses_git_object_identity(self):
        self.assertEqual(git_blob(b''), 'e69de29bb2d1d6434b8b29ae775ad8c2e48c5391')

    def test_length_and_bytes_are_bound(self):
        self.assertNotEqual(git_blob(b'a'), git_blob(b'a\0'))
        self.assertNotEqual(git_blob(b'ab'), git_blob(b'ba'))

    def test_export_reuses_snapshot_with_target_and_gates(self):
        root = admission.Path('/private/source')
        private = admission.Path('/private')
        target = root / 'target'
        effigy = root / '.effigy'
        paths = [root / 'Cargo.lock']
        contents = {'Cargo.lock': b'lock'}
        def metadata(path):
            return mock.Mock(st_mode=0o100644 if path.name == 'Cargo.lock' else 0o040700)
        def children(path, pattern):
            if path == effigy:
                gates = effigy / 'state/gates'
                scope = 'a' * 32
                return iter([effigy / 'state', gates, gates / (scope + '.scope'),
                             gates / scope, gates / scope / (('b' * 32) + '.gate')])
            return iter(())
        with mock.patch.object(admission.Path, 'iterdir', lambda _: iter(paths)), \
                mock.patch.object(admission.Path, 'rglob', children), \
                mock.patch.object(admission.Path, 'lstat', metadata), \
                mock.patch.object(admission.Path, 'resolve', lambda self, strict=True: self), \
                mock.patch.object(admission.Path, 'is_symlink', return_value=False), \
                mock.patch.object(admission.Path, 'is_dir', lambda self: self.name != 'Cargo.lock'), \
                mock.patch.object(admission.Path, 'is_file', lambda self: self.name == 'Cargo.lock'), \
                mock.patch.object(admission.Path, 'stat', lambda self: metadata(self)), \
                mock.patch.object(admission.Path, 'read_bytes', lambda self: contents[self.name]), \
                mock.patch.object(admission, 'validate_generated_output') as target_check, \
                mock.patch.object(admission, 'validate_foreground_gate', return_value=True) as gate_check:
            members = {'Cargo.lock': 'file'}
            before = export.inventory(root, members)
            baseline = admission.source_snapshot(root)
            paths.extend([target, effigy])
            export.verify_source_after_stage(root, members, before, private, baseline)
            target_check.assert_called_once_with(target, private)
            self.assertEqual(gate_check.call_count, 6)
            contents['Cargo.lock'] = b'changed'
            with self.assertRaises(RuntimeError):
                export.verify_source_after_stage(root, members, before, private, baseline)


if __name__ == '__main__':
    unittest.main()
