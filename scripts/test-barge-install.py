"""Synthetic installer fixtures. All mutable paths live below caller TMPDIR."""
import datetime
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock

spec = importlib.util.spec_from_file_location("installer", Path(__file__).with_name("barge-install.py"))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallerFixtures(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="barge-fixture-", dir=os.environ["TMPDIR"])
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.home = root / "home"
        (self.home / ".local/bin").mkdir(parents=True)
        self.dest = self.home / ".local/bin/barge"
        self.receipts = self.home / ".local/state/barge/install"
        self.patches = [patch.object(installer, "HOME", self.home),
                        patch.object(installer, "DESTINATION", self.dest),
                        patch.object(installer, "RECEIPTS", self.receipts),
                        patch.dict(os.environ, {"PATH": str(root / "empty")})]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)

    def test_initial_refusals(self):
        self.assertEqual(installer.preflight(False), (None, None))
        self.dest.write_text("foreign")
        with self.assertRaises(installer.Refusal):
            installer.preflight(False)
        self.dest.unlink()
        other = Path(self.temp.name) / "empty"
        other.mkdir()
        (other / "barge").symlink_to("missing")
        with self.assertRaises(installer.Refusal):
            installer.preflight(False)

    def test_receipts_and_replacement(self):
        built = Path(self.temp.name) / "built"
        built.write_bytes(b"first")
        _, installed = installer.install(built, None)
        started = datetime.datetime.now(datetime.timezone.utc)
        receipt = {"installed": installed, "build_instance": "a" * 32}
        first = installer.write_receipt(receipt, started, "b" * 40)
        previous, expected = installer.preflight(True)
        self.assertEqual(previous[0], first)
        built.write_bytes(b"second")
        _, installed = installer.install(built, expected)
        receipt = {"installed": installed, "build_instance": "c" * 32}
        second = installer.write_receipt(receipt, started + datetime.timedelta(microseconds=1), "b" * 40)
        self.assertEqual(json.loads(second.read_text())["installed"]["sha256"], installer.sha256(self.dest))
        self.assertTrue(first.exists())
        self.dest.write_bytes(b"tampered")
        with self.assertRaises(installer.Refusal):
            installer.preflight(True)

    def test_pin_and_identity_refuse(self):
        for pin in (None, "main", "a" * 39):
            with self.assertRaises(installer.Refusal):
                installer.authenticate_pin(pin)
        fake = Path(self.temp.name) / "fake"
        fake.write_text('#!/bin/sh\necho "{}"\n')
        fake.chmod(0o700)
        with self.assertRaises(installer.Refusal):
            installer.build_identity(fake, "a" * 40, "b" * 32)

    def test_pin_authentication(self):
        pin, tip, tree = "a" * 40, "b" * 40, "c" * 40
        answers = [f"{tip} refs/heads/main", "", tip, pin, tree]
        with patch.object(installer, "git", side_effect=answers), \
             patch.object(installer.subprocess, "run", return_value=Mock(returncode=0)):
            self.assertEqual(installer.authenticate_pin(pin), (pin, tree, tip))
        with patch.object(installer, "git", side_effect=[f"{tip} refs/heads/main", "", pin]):
            with self.assertRaises(installer.Refusal):
                installer.authenticate_pin(pin)
        with patch.object(installer, "git", side_effect=[f"{tip} refs/heads/main", "", tip, pin]), \
             patch.object(installer.subprocess, "run", return_value=Mock(returncode=1)):
            with self.assertRaises(installer.Refusal):
                installer.authenticate_pin(pin)

    def test_environment_isolated(self):
        staging = Path(self.temp.name) / "staging"
        staging.mkdir()
        with patch.dict(os.environ, {"CARGO_TARGET_DIR": "/foreign", "OXIDE_POOL": "/foreign",
                                      "CARGO_BUILD_BUILD_DIR": "/foreign"}):
            environment = installer.build_environment(staging, self.home, "a" * 40, "b" * 32)
        self.assertNotIn("CARGO_TARGET_DIR", environment)
        self.assertNotIn("CARGO_BUILD_BUILD_DIR", environment)
        self.assertEqual(environment["OXIDE_POOL"], str(self.home / "p"))
        self.assertEqual(environment["TMPDIR"], str(staging))
        self.assertEqual(environment["BARGE_BUILD_PIN"], "a" * 40)

    def test_pool_verification_and_identity(self):
        pool = Path(self.temp.name) / "pool"
        pool.mkdir(mode=0o700)
        device = pool.stat().st_dev
        # Fixture paths can be longer than the actual short boot pool.
        with patch.object(installer.os, "fsencode", return_value=b"short"):
            identity = installer.verify_pool(pool, device)
            with self.assertRaises(installer.Refusal):
                installer.verify_pool(pool, device + 1)
            with self.assertRaises(installer.Refusal):
                installer.verify_pool(pool, device, (identity[0], identity[1] + 1))
            pool.chmod(0o755)
            with self.assertRaises(installer.Refusal):
                installer.verify_pool(pool, device)
        pool.chmod(0o700)
        with patch.object(installer.os, "fsencode", return_value=b"x" * 92):
            with self.assertRaises(installer.Refusal):
                installer.verify_pool(pool, device)
        self.assertTrue(pool.exists())

    def test_pool_cleanup_requires_released_ledger(self):
        pool = Path(self.temp.name) / "pool"
        (pool / "p").mkdir(parents=True, mode=0o700)
        pool.chmod(0o700)
        info = pool.stat()
        expected = (info.st_dev, info.st_ino)
        ledger = pool / "p/ledger.json"
        ledger.write_text(json.dumps({"version": 1, "slots": [{"state": "held", "lease_id": "x"}]}))
        real_stat = os.stat
        with patch.object(installer.os, "fsencode", return_value=b"short"), \
             patch.object(installer.os, "stat") as stat_mock:
            root_info = Mock(st_dev=info.st_dev)
            stat_mock.side_effect = lambda path, *a, **kw: root_info if path == "/" else real_stat(path, *a, **kw)
            with self.assertRaises(installer.Refusal):
                installer.remove_released_pool(pool, expected)
            self.assertTrue(pool.exists())
            ledger.write_text(json.dumps({"version": 1, "slots": [{"state": "idle", "lease_id": None}]}))
            installer.remove_released_pool(pool, expected)
            self.assertFalse(pool.exists())

    def test_unsafe_receipt_and_missing_receipt_refuse(self):
        with self.assertRaises(installer.Refusal):
            installer.preflight(True)
        self.receipts.mkdir(parents=True)
        (self.receipts / "z.json").symlink_to(self.dest)
        with self.assertRaises(installer.Refusal):
            installer.preflight(True)

    def test_changed_destination_refuses(self):
        built = Path(self.temp.name) / "built"
        built.write_bytes(b"binary")
        self.dest.write_bytes(b"other")
        with self.assertRaises(installer.Refusal):
            installer.install(built, None)
        self.assertEqual(self.dest.read_bytes(), b"other")


if __name__ == "__main__":
    unittest.main()
