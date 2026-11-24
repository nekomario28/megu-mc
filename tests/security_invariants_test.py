#!/usr/bin/env python3
"""Mutation tests for the structural security guard (stdlib only)."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from tools.security_invariants_audit import FILES

AUDIT = ROOT / "tools/security_invariants_audit.py"
MUTATIONS = (
    ("src/play_protocol.rs", "|| allow_insecure_offline_public", "|| false"),
    ("src/server_reliability.rs", "if !ct_tag_eq(tag, &hmac_sha256(secret, body))", "if tag != &hmac_sha256(secret, body)"),
    ("src/connection_limits.rs", "std::hint::black_box(diff) == 0", "a == b"),
    ("src/ingress_guard.rs", "2 * 1024 * 1024 - 1", "64 * 1024 * 1024"),
    ("src/connection_limits.rs", "self.inflate.total_out() != length as u64", "false"),
    ("src/nbt.rs", "if depth >= 512", "if depth >= 999999"),
    ("mod-adapter-core/src/mod_sync.rs", "if !has_suffix_ci(&entry.file_name, \".mmod\")", "if false"),
    ("client-app/src/integrated.rs", "Ipv4Addr::LOCALHOST", "Ipv4Addr::UNSPECIFIED"),
)


def invoke(root):
    result = subprocess.run([sys.executable, str(AUDIT), "--root", str(root), "--json"],
                            capture_output=True, text=True, check=False)
    return result, json.loads(result.stdout)


class SecurityInvariantsTest(unittest.TestCase):
    def test_live_tree(self):
        result, report = invoke(ROOT)
        self.assertEqual(result.returncode, 0, report)
        self.assertTrue(all(item["pass"] for item in report["checks"]))

    def test_every_guard_fails_closed_when_weakened(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in FILES:
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.symlink_to(ROOT / name)
            for name, before, after in MUTATIONS:
                with self.subTest(file=name, guard=before):
                    target = root / name
                    text = (ROOT / name).read_text()
                    self.assertIn(before, text)
                    target.unlink()
                    target.write_text(text.replace(before, after, 4 if name == "src/nbt.rs" else 1))
                    result, report = invoke(root)
                    self.assertEqual(result.returncode, 1, report)
                    self.assertTrue(any(not item["pass"] for item in report["checks"]))
                    target.unlink()
                    target.symlink_to(ROOT / name)
            (root / "src/ingress_guard.rs").unlink()
            result, report = invoke(root)
            self.assertEqual(result.returncode, 1, report)
            self.assertIn("src/ingress_guard.rs", report["errors"])

    def test_production_forbidden_calls_fail_but_test_code_is_exempt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in FILES:
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.symlink_to(ROOT / name)
            target = root / "src/ingress_guard.rs"
            original = target.read_text()
            target.unlink()
            for forbidden in ("SystemTime::now()", "x.lock().unwrap()"):
                with self.subTest(forbidden=forbidden):
                    target.write_text(f"fn injected() {{ let _ = {forbidden}; }}\n" + original)
                    result, report = invoke(root)
                    self.assertEqual(result.returncode, 1, report)
                    target.write_text(original + f"\n#[cfg(test)]\nmod audit_test_only {{\n fn example() {{ let _ = {forbidden}; }}\n}}\n")
                    result, report = invoke(root)
                    self.assertEqual(result.returncode, 0, report)


if __name__ == "__main__":
    unittest.main(verbosity=2)
