#!/usr/bin/env python3
"""Synthetic-tree tests for the audiovisual reachability audit."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
AUDIT = ROOT / "tools/wiring_audit.py"


def put(root, name, content):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def put_json(root, name, payload):
    put(root, name, json.dumps(payload, indent=2) + "\n")


def features(*entries):
    return {"version": 1, "features": list(entries)}


def feature(symbol, file, consumers, via=None):
    entry = {"symbol": symbol, "file": file, "consumers": consumers}
    if via:
        entry["via"] = via
    return entry


def run(root, *args):
    return subprocess.run(
        [sys.executable, str(AUDIT), "--root", str(root), "--av-reachability", *args],
        text=True, capture_output=True, check=False, timeout=30,
    )


def base_tree(root):
    put(root, "src/lib.rs", "")


class WiringAuditAvTests(unittest.TestCase):
    def test_test_only_symbol_is_flagged_unreachable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base_tree(root)
            put(root, "client-app/src/feature.rs", "pub fn dead_feature() {}\n")
            put(root, "client-app/src/session.rs", "pub fn run() {}\n")
            put(root, "client-app/tests/feature_tests.rs", """
                use mc_spine_client::feature::dead_feature;
                #[test]
                fn check() { dead_feature(); }
            """)
            put_json(root, "tools/wiring_audit_features.json", features(
                feature("dead_feature", "client-app/src/feature.rs",
                        ["client-app/src/session.rs"]),
            ))
            advisory = run(root)
            self.assertEqual(advisory.returncode, 0, advisory.stdout + advisory.stderr)
            self.assertIn("unreachable", advisory.stdout)
            self.assertIn("dead_feature", advisory.stdout)
            strict = run(root, "--strict")
            self.assertEqual(strict.returncode, 1, strict.stdout + strict.stderr)
            self.assertIn("dead_feature", strict.stdout)

    def test_symbol_called_from_consumer_root_is_wired(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base_tree(root)
            put(root, "client-port/src/live.rs", "pub fn live_feature() {}\n")
            put(root, "client-gpu/src/renderer.rs", """
                use mc_spine_client_mesh::live::live_feature;
                pub fn render() { live_feature(); }
            """)
            put_json(root, "tools/wiring_audit_features.json", features(
                feature("live_feature", "client-port/src/live.rs",
                        ["client-gpu/src/renderer.rs"]),
            ))
            result = run(root, "--strict")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("1/1 wired", result.stdout)
            self.assertIn("live_feature", result.stdout)
            self.assertIn("client-gpu/src/renderer.rs", result.stdout)

    def test_stale_allowlist_entry_fails_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base_tree(root)
            put(root, "client-port/src/live.rs", "pub fn live_feature() {}\n")
            put(root, "client-gpu/src/renderer.rs", """
                use mc_spine_client_mesh::live::live_feature;
                pub fn render() { live_feature(); }
            """)
            put_json(root, "tools/wiring_audit_features.json", features(
                feature("live_feature", "client-port/src/live.rs",
                        ["client-gpu/src/renderer.rs"]),
            ))
            put_json(root, "tools/wiring_audit_allowlist.json", {
                "version": 1,
                "entries": [{
                    "symbol": "live_feature",
                    "file": "client-port/src/live.rs",
                    "reason": "synthetic stale entry",
                    "date": "2026-11-22",
                }],
            })
            result = run(root, "--strict")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("STALE allowlist", result.stdout)
            self.assertIn("live_feature", result.stdout)

    def test_comment_and_string_mentions_are_not_call_sites(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base_tree(root)
            put(root, "client-app/src/commented.rs", "pub fn commented_feature() {}\n")
            put(root, "client-app/src/session.rs", """
                use crate::commented::commented_feature;
                // commented_feature();
                const EXAMPLE: &str = "commented_feature";
                const CALL_SHAPE: &str = "session.commented_feature()";
                /* commented_feature(); */
                pub fn run() { let _ = EXAMPLE; }
            """)
            put_json(root, "tools/wiring_audit_features.json", features(
                feature("commented_feature", "client-app/src/commented.rs",
                        ["client-app/src/session.rs"]),
            ))
            advisory = run(root)
            self.assertEqual(advisory.returncode, 0, advisory.stdout + advisory.stderr)
            self.assertIn("unreachable", advisory.stdout)
            self.assertIn("commented_feature", advisory.stdout)
            strict = run(root, "--strict")
            self.assertEqual(strict.returncode, 1, strict.stdout + strict.stderr)


if __name__ == "__main__":
    unittest.main()
