#!/usr/bin/env python3
"""Executable-input and temporary-space boundaries for native reference probes."""
import errno
import hashlib
import importlib.util
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("vanilla_java", ROOT / "tools/vanilla_java.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class ProbeBoundary(unittest.TestCase):
    def test_bad_pin_is_rejected_before_creating_a_workspace(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar = Path(tmp) / "bad.jar"
            jar.write_bytes(b"not the pinned executable")
            with patch.object(native.tempfile, "TemporaryDirectory") as create:
                with self.assertRaisesRegex(ValueError, "pinned"):
                    with native.pinned_runtime(jar):
                        self.fail("unauthenticated payload accepted")
                create.assert_not_called()

    def test_space_is_checked_before_any_archive_member_is_written(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar = Path(tmp) / "synthetic.jar"
            with zipfile.ZipFile(jar, "w") as archive:
                archive.writestr("META-INF/versions/1.21.1/server-1.21.1.jar", b"game")
                archive.writestr("META-INF/libraries/test.jar", b"library")
            # A synthetic, never-executed archive isolates the unpacking guard.
            digest = hashlib.sha1(jar.read_bytes()).hexdigest()
            with patch.object(native, "JAR_SHA1", digest), \
                    patch.object(native.shutil, "disk_usage", return_value=SimpleNamespace(free=0)), \
                    patch.object(Path, "write_bytes") as write:
                with self.assertRaisesRegex(OSError, "TMPDIR") as error:
                    with native.pinned_runtime(jar):
                        self.fail("insufficient workspace accepted")
                self.assertEqual(error.exception.errno, errno.ENOSPC)
                write.assert_not_called()

    def test_shared_native_helpers_are_compiled_with_the_probe(self):
        with patch.object(native.subprocess, "run") as run:
            native.run_probe(Path("/work"), "classpath", Path("Probe.java"), "Main", "out",
                             extra_sources=[Path("Tags.java")])
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[0].args[0][-2:], ["Probe.java", "Tags.java"])
        self.assertEqual(run.call_args_list[1].args[0][-2:], ["Main", "out"])


if __name__ == "__main__":
    unittest.main()
