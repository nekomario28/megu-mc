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
    ("proto/src/wire.rs", "PROTOCOL_767_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024 - 1", "PROTOCOL_767_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024"),
    ("src/play_protocol.rs", "const MAX_LOGIN_FRAME: usize = 64 * 1024;", "const MAX_LOGIN_FRAME: usize = 64 * 1024 * 1024;"),
    ("src/connection_limits.rs", "MAX_UNCOMPRESSED_PACKET_BYTES: usize = 8 * 1024 * 1024", "MAX_UNCOMPRESSED_PACKET_BYTES: usize = 64 * 1024 * 1024"),
    ("src/connection_limits.rs", "unwrap_or(15)\n            .max(1)", "unwrap_or(0)\n            .max(1)"),
    ("src/play_protocol.rs", "let mut authenticated = false;", "let mut authenticated = true;"),
    ("proto/src/session.rs", "login_acknowledged before login_success", "login_acknowledged accepted before login_success"),
    ("wasm-mod-runtime/src/exec.rs", "checked_sub(1).ok_or(Trap::FuelExhausted)?", "saturating_sub(1)"),
    ("wasm-mod-runtime/src/wasm_mod.rs", "MAX_MEM_PAGES_HARD_CAP: u32 = 1024;", "MAX_MEM_PAGES_HARD_CAP: u32 = u32::MAX;"),
    ("wasm-mod-runtime/src/exec.rs", "if len > self.max_string", "if false && len > self.max_string"),
    ("server-files/src/bin/megu_installer.rs", "if actual != expected", "if false && actual != expected"),
    ("server-files/src/modrinth_sync.rs", "\"..\" => return Err(PathRejection::ParentTraversal),", "\"..\" => {},"),
    ("core/src/crash_report.rs", "out = redact_bearer_tokens(&out);", "out = out.clone();"),
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
