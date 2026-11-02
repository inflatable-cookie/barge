#!/usr/bin/env python3
"""Pure source controls for the held tape export producer."""
import unittest
from export import git_blob


class GitBlobIdentity(unittest.TestCase):
    def test_empty_blob_uses_git_object_identity(self):
        self.assertEqual(git_blob(b''), 'e69de29bb2d1d6434b8b29ae775ad8c2e48c5391')

    def test_length_and_bytes_are_bound(self):
        self.assertNotEqual(git_blob(b'a'), git_blob(b'a\0'))
        self.assertNotEqual(git_blob(b'ab'), git_blob(b'ba'))


if __name__ == '__main__':
    unittest.main()
