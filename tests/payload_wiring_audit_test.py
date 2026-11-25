# SPDX-License-Identifier: LGPL-3.0-or-later
"""Regression checks for the permanent generation-payload wiring audit."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("payload_wiring_audit", ROOT / "tools/payload_wiring_audit.py")
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)


class PayloadWiringAuditTest(unittest.TestCase):
    def test_repository_wiring_is_clean(self):
        self.assertEqual(AUDIT.findings(), [])

    def test_strict_command_passes(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "tools/payload_wiring_audit.py"), "--strict"],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
