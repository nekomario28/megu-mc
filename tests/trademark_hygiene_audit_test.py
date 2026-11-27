#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Fixture-driven tests for tools/trademark_hygiene_audit.py.

The audit scans the first-party UI/engine surface for third-party mod marks
used as Megu's own feature, module or public-API names. These tests prove the
newly covered crates (client-gpu, client-kernel, state-store, mod-offload,
wasm-mod-runtime) fail closed on a regression, that masked test code cannot
trip the gate, and that the documented compatibility crates stay out of
scope until their nominative uses carry explicit allow markers.
"""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "tools" / "trademark_hygiene_audit.py"
spec = importlib.util.spec_from_file_location("trademark_hygiene_audit", SCRIPT)
AUDIT = importlib.util.module_from_spec(spec)
spec.loader.exec_module(AUDIT)


class TrademarkHygieneTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def source(self, path, contents):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(contents, encoding="utf-8")

    def scan(self):
        return AUDIT.scan(self.root)

    def kinds(self, findings):
        return sorted({f["kind"] for f in findings})

    def test_newly_covered_crates_fail_on_mod_marks(self):
        for crate in ("client-gpu", "client-kernel", "state-store",
                      "mod-offload", "wasm-mod-runtime"):
            with self.subTest(crate=crate):
                self.source(f"{crate}/src/widget.rs",
                            'fn label() -> &\'static str { "Sodium" }\n')
                findings = self.scan()
                self.assertTrue(findings, f"{crate} did not report a finding")
                self.assertIn("string-literal", self.kinds(findings))
                # Reset for the next crate.
                (self.root / f"{crate}/src/widget.rs").unlink()

    def test_public_api_name_in_new_crate_fails(self):
        self.source("client-kernel/src/lib.rs",
                    "pub fn jei_overlay_state() -> bool { true }\n")
        findings = self.scan()
        self.assertTrue(any(f["kind"] == "public-api-name" for f in findings))

    def test_enum_variant_in_new_crate_fails(self):
        self.source("state-store/src/kind.rs",
                    "pub enum PackKind {\n    IrisShaders,\n}\n")
        findings = self.scan()
        self.assertTrue(any(f["kind"] == "public-enum-variant" for f in findings))

    def test_cfg_test_module_cannot_trip_gate(self):
        self.source("client-gpu/src/widget.rs",
                    "#[cfg(test)]\nmod tests {\n"
                    "    fn fixture() { let _ = \"optifine\"; }\n}\n"
                    "fn label() -> &str { \"overlay\" }\n")
        self.assertEqual([], self.scan())

    def test_documented_compat_crates_stay_out_of_scope(self):
        # client-port carries reviewed Iris/OptiFine compatibility code; it is
        # deliberately unscanned until those uses carry allow markers. A mark
        # there must NOT produce a finding today (and must once the crate is
        # onboarded — see the audit's scanned_roots comment).
        self.source("client-port/src/shaderpack.rs",
                    "pub enum Kind {\n    IrisModern,\n}\n")
        self.assertEqual([], self.scan())

    def test_allow_marker_suppresses_finding(self):
        self.source("client-kernel/src/alias.rs",
                    "fn label() -> &str {\n"
                    "    \"mouse tweaks\" // trademark-hygiene:allow search alias for upstream pack names\n"
                    "}\n")
        self.assertEqual([], self.scan())

    def test_live_repository_is_clean(self):
        completed = subprocess.run(
            [sys.executable, str(SCRIPT), "--strict", "--root", str(REPO_ROOT)],
            capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("clean", completed.stdout)

    def test_scanned_roots_cover_every_clean_first_party_crate(self):
        report = AUDIT.scan(REPO_ROOT)
        self.assertEqual([], report)
        payload = json.loads(subprocess.run(
            [sys.executable, str(SCRIPT), "--json", "--root", str(REPO_ROOT)],
            capture_output=True, text=True, check=True).stdout)
        roots = set(payload["scanned_roots"])
        for crate in ("client-gpu/src", "client-kernel/src", "state-store/src",
                      "mod-offload/src", "wasm-mod-runtime/src"):
            self.assertIn(crate, roots)


if __name__ == "__main__":
    unittest.main()
