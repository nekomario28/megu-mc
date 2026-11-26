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
from tools.security_invariants_audit import FILES, PLAY, SESSION

AUDIT = ROOT / "tools/security_invariants_audit.py"
EXEC = "wasm-mod-runtime/src/exec.rs"
CAP_CHECK = "if len > self.max_string"
UNCAPPED_CHECK = "if false && len > self.max_string"
MUTATIONS = {
    "offline public bind refusal": [(PLAY, "|| allow_insecure_offline_public", "|| false")],
    "constant-time Velocity HMAC": [
        ("src/server_reliability.rs", "if !ct_tag_eq(tag, &hmac_sha256(secret, body))", "if tag != &hmac_sha256(secret, body)")],
    "constant-time RCON comparison": [("src/connection_limits.rs", "std::hint::black_box(diff) == 0", "a == b")],
    "bounded play frame before allocation": [("src/ingress_guard.rs", "2 * 1024 * 1024 - 1", "64 * 1024 * 1024")],
    "exact bounded decompression": [("src/connection_limits.rs", "self.inflate.total_out() != length as u64", "false")],
    "NBT container depth cap": [("src/nbt.rs", "if depth >= 512", "if depth >= 999999")],
    "server push rejects JARs": [("mod-adapter-core/src/mod_sync.rs", 'if !has_suffix_ci(&entry.file_name, ".mmod")', "if false")],
    "integrated loopback ephemeral port": [("client-app/src/integrated.rs", "Ipv4Addr::LOCALHOST", "Ipv4Addr::UNSPECIFIED")],
    "canonical protocol 767 frame cap": [
        ("proto/src/wire.rs", "PROTOCOL_767_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024 - 1", "PROTOCOL_767_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024")],
    "pre-login frame cap before allocation": [(PLAY, "const MAX_LOGIN_FRAME: usize = 64 * 1024;", "const MAX_LOGIN_FRAME: usize = 64 * 1024 * 1024;")],
    "absolute 8 MiB decompression ceiling": [
        ("src/connection_limits.rs", "MAX_UNCOMPRESSED_PACKET_BYTES: usize = 8 * 1024 * 1024", "MAX_UNCOMPRESSED_PACKET_BYTES: usize = 64 * 1024 * 1024")],
    "pre-login packet rate and slowloris deadlines": [
        ("src/connection_limits.rs", "unwrap_or(15)\n            .max(1)", "unwrap_or(0)\n            .max(1)"),
        (SESSION, "set_read_timeout(Some(Duration::from_secs(10)))", "set_read_timeout(None)"),
        (PLAY, "if rate.allow(now) {\n            return Some(packet);", "if true {\n            return Some(packet);"),
        (PLAY, "deadline.checked_duration_since(Instant::now())?", "Duration::from_secs(10)"),
    ],
    "verified UUID binding before admission": [
        (PLAY, "let Some(preplay) = negotiate_preplay_session(s)", "let Some(preplay) = None"),
        (SESSION, "let mut authenticated = false;", "let mut authenticated = true;"),
        (SESSION, "uuid = fwd.uuid;", "uuid = uuid;"),
        (SESSION, "uuid = profile.uuid;", "uuid = uuid;"),
        (SESSION, "&live.whitelist,\n            authenticated,", "&live.whitelist,\n            false,"),
    ],
    "login state machine cannot skip success": [
        ("proto/src/session.rs", "login_acknowledged before login_success", "login_acknowledged accepted before login_success")],
    "WASM fuel ceiling enforcement": [(EXEC, "checked_sub(1).ok_or(Trap::FuelExhausted)?", "saturating_sub(1)")],
    "WASM memory hard ceiling": [("wasm-mod-runtime/src/wasm_mod.rs", "MAX_MEM_PAGES_HARD_CAP: u32 = 1024;", "MAX_MEM_PAGES_HARD_CAP: u32 = u32::MAX;")],
    "WASM hostcall memory and string caps": [
        # Mutate each host binding independently, not both at once.
        *((EXEC, arg + CAP_CHECK, arg + UNCAPPED_CHECK)
          for arg in ("args.get(1).map_or(0, |v| v.i32()) as u32;\n                ",
                      "args.get(2).map_or(0, |v| v.i32()) as u32;\n                ")),
        (EXEC, "self.data[self.host_range(ptr, len)?]", "self.data[ptr as usize..]"),
        (EXEC, "if end > self.data.len() as u64 {\n            return Err(Trap::HostMemoryAccess",
         "if false {\n            return Err(Trap::HostMemoryAccess"),
    ],
    "installer hash verification fail closed": [("server-files/src/bin/megu_installer.rs", "if actual != expected", "if false && actual != expected")],
    "mrpack/installer path traversal rejection": [("server-files/src/modrinth_sync.rs", '".." => return Err(PathRejection::ParentTraversal),', '".." => {},')],
    "crash report redacts PII before output/fingerprint": [("core/src/crash_report.rs", "out = redact_bearer_tokens(&out);", "out = out.clone();")],
}


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
            for guard, mutations in MUTATIONS.items():
                for name, before, after in mutations:
                    with self.subTest(guard=guard, file=name, anchor=before):
                        target = root / name
                        text = (ROOT / name).read_text()
                        self.assertIn(before, text)
                        target.unlink()
                        try:
                            target.write_text(text.replace(before, after, 4 if name == "src/nbt.rs" else 1))
                            result, report = invoke(root)
                            self.assertEqual(result.returncode, 1, report)
                            self.assertIn({"name": guard, "pass": False}, report["checks"])
                            self.assertFalse(report["errors"])
                        finally:
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
