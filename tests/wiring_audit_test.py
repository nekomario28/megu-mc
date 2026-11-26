#!/usr/bin/env python3
"""Structural wiring audit fixtures, independent of Cargo or external crates."""

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


def run(root, *args):
    return subprocess.run(
        [sys.executable, str(AUDIT), "--root", str(root), *args],
        text=True, capture_output=True, check=False, timeout=120,
    )


class WiringAuditTests(unittest.TestCase):
    def test_wired_from_runtime_paths_passes_strict(self):
        for consumer in ("session", "mesher", "main"):
            with self.subTest(consumer=consumer), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                put(root, "src/foo.rs", "pub struct FooWidget;\npub fn activate_foo() {}\n")
                put(root, "src/lib.rs", "pub mod foo;\n")
                if consumer == "main":
                    put(root, "src/main.rs", "fn main() { crate::foo::activate_foo(); }\n")
                else:
                    put(root, "src/lib.rs", f"pub mod foo;\npub mod {consumer};\n")
                    put(root, f"src/{consumer}.rs",
                        "use crate::foo::FooWidget;\n"
                        "pub fn launch() { let _instance = FooWidget; }\n")
                    put(root, "src/main.rs", f"fn main() {{ crate::{consumer}::launch(); }}\n")
                result = run(root, "--strict")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("0 unwired", result.stdout)

    def test_island_declared_and_tested_but_not_consumed_fails_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/lib.rs", "pub mod foo;\n")
            put(root, "src/foo.rs", "pub struct FooWidget;\npub fn activate_foo() {}\n")
            put(root, "tests/foo_tests.rs", "use test_crate::foo::FooWidget;\n")
            result = run(root, "--strict")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("UNWIRED module src/foo.rs", result.stdout)
            pending = run(root, "--strict", "--report-only")
            self.assertEqual(pending.returncode, 0, pending.stdout + pending.stderr)
            self.assertIn("1 unwired", pending.stdout)

    def test_test_blocks_and_comments_do_not_supply_wiring(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/lib.rs", "pub mod foo;\npub mod consumer;\n")
            put(root, "src/foo.rs", "pub struct FooWidget;\n")
            put(root, "src/consumer.rs", """
                // use crate::foo::FooWidget;
                const EXAMPLE: &str = "crate::foo::FooWidget";
                /* use crate::foo::FooWidget; /* nested test example */ */
                #[cfg(test)] mod tests {
                    use crate::foo::FooWidget;
                    #[test] fn check() { let _ = FooWidget; }
                }
                pub fn run() { let _ = EXAMPLE; }
            """)
            put(root, "src/main.rs", "fn main() { crate::consumer::run(); }\n")
            result = run(root, "--strict")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("UNWIRED module src/foo.rs", result.stdout)
            self.assertNotIn("UNWIRED module src/consumer.rs", result.stdout)

    def test_included_server_source_is_reachable_and_test_only_source_is_excluded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/lib.rs", "")
            put(root, "src/main.rs", """
                include!("server_tick.rs");
                #[cfg(test)] mod test_support;
                fn main() { server_tick(); }
            """)
            put(root, "src/server_tick.rs", "fn server_tick() {}\n")
            put(root, "src/test_support.rs", "pub fn test_helper() {}\n")
            result = run(root, "--format", "json", "--strict")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            modules = {item["module"]: item for item in json.loads(result.stdout)["modules"]}
            self.assertIn("src/main.rs", modules["src/server_tick.rs"]["referenced_by"])
            self.assertNotIn("src/test_support.rs", modules)

    def test_unrelated_exported_constant_name_is_not_wiring(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/lib.rs", "pub mod foo;\npub mod other;\n")
            put(root, "src/foo.rs", "pub const PAGE_SIZE: usize = 4096;\n")
            put(root, "src/other.rs", """
                const PAGE_SIZE: usize = 8192;
                pub fn run() -> usize { PAGE_SIZE }
            """)
            put(root, "src/main.rs", "fn main() { crate::other::run(); }\n")
            result = run(root, "--strict")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("UNWIRED module src/foo.rs", result.stdout)
            self.assertNotIn("UNWIRED module src/other.rs", result.stdout)

    def test_public_reexport_alias_wires_its_defining_module(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/main.rs", "fn main() {}\n")
            put(root, "proto/Cargo.toml", '[package]\nname = "megu-proto"\n')
            put(root, "proto/src/lib.rs", "pub mod resolver;\npub use resolver::{resolve as resolve_server_address};\n")
            put(root, "proto/src/resolver.rs", "pub fn resolve() {}\n")
            put(root, "client-app/Cargo.toml", '[package]\nname = "client-app"\n')
            put(root, "client-app/src/lib.rs", "pub mod protocol;\n")
            put(root, "client-app/src/protocol.rs", "pub fn run() { megu_proto::resolve_server_address(); }\n")
            put(root, "client-app/src/main.rs", "fn main() { crate::protocol::run(); }\n")
            result = run(root, "--format", "json", "--strict")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            modules = {item["module"]: item for item in json.loads(result.stdout)["modules"]}
            self.assertIn("client-app/src/protocol.rs", modules["proto/src/resolver.rs"]["referenced_by"])

    def test_binary_and_tools_are_consumer_sources_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/main.rs", "fn main() {}\n")
            put(root, "server-files/Cargo.toml", '[package]\nname = "megu-server-files"\n')
            put(root, "server-files/src/lib.rs", "pub mod admin_cli;\n")
            put(root, "server-files/src/admin_cli.rs", "pub fn run() {}\n")
            put(root, "server-files/src/bin/admin.rs", "fn main() { megu_server_files::admin_cli::run(); }\n")
            put(root, "mod-offload/Cargo.toml", '[package]\nname = "mod-offload"\n')
            put(root, "mod-offload/src/lib.rs", "pub mod ffi;\n")
            put(root, "mod-offload/src/ffi.rs", 'pub unsafe extern "C" fn mso_region_create() {}\n')
            put(root, "tools/offload_probe/Cargo.toml", '[package]\nname = "offload-probe"\n')
            put(root, "tools/offload_probe/src/main.rs", "use mod_offload::ffi::*;\nfn main() { unsafe { mso_region_create(); } }\n")
            result = run(root, "--format", "json", "--strict")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            modules = {item["module"]: item for item in json.loads(result.stdout)["modules"]}
            self.assertIn("server-files/src/bin/admin.rs", modules["server-files/src/admin_cli.rs"]["referenced_by"])
            self.assertIn("tools/offload_probe/src/main.rs", modules["mod-offload/src/ffi.rs"]["referenced_by"])
            self.assertNotIn("server-files/src/bin/admin.rs", modules)

    def test_cdylib_exported_c_abi_is_a_boundary_consumer(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/main.rs", "fn main() {}\n")
            put(root, "client-kernel/Cargo.toml", '[package]\nname = "megu-kernel"\n[lib]\ncrate-type = ["rlib", "cdylib"]\n')
            put(root, "client-kernel/src/lib.rs", "mod ffi;\n")
            put(root, "client-kernel/src/ffi.rs", '#[no_mangle]\npub extern "C" fn megu_kernel_abi_version() -> u32 { 1 }\n')
            result = run(root, "--format", "json", "--strict")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            modules = {item["module"]: item for item in json.loads(result.stdout)["modules"]}
            self.assertEqual(modules["client-kernel/src/ffi.rs"]["referenced_by"],
                             ["client-kernel/src/ffi.rs"])

    def test_contract_requires_declared_live_consumer(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            put(root, "src/main.rs", "fn main() {}\n")
            put(root, "client-ui/src/mod_manager.rs", "pub struct ModManagerWidget;\n")
            put(root, "client-ui/src/lib.rs", "pub mod mod_manager;\n")
            put(root, "client-ui/src/other.rs", "use crate::mod_manager::ModManagerWidget;\n")
            result = run(root, "--format", "json", "--strict")
            self.assertEqual(result.returncode, 1, result.stderr)
            report = json.loads(result.stdout)
            contract = next(c for c in report["contracts"] if c["module"] == "client-ui/mod_manager")
            self.assertEqual(contract["status"], "pending")
            put(root, "client-ui/src/layout.rs", "use crate::mod_manager::ModManagerWidget;\n")
            result = run(root, "--format", "json", "--report-only")
            report = json.loads(result.stdout)
            contract = next(c for c in report["contracts"] if c["module"] == "client-ui/mod_manager")
            self.assertEqual(contract["status"], "wired")

    def test_real_repository_json(self):
        result = run(ROOT, "--format", "json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertIn("contracts", report)
        self.assertIn("modules", report)
        self.assertGreater(report["summary"]["modules_total"], 0)
        self.assertGreater(report["summary"]["contracts_total"], 0)
        for crate in (
            "core", "proto", "state-store", "server-files", "mod-offload",
            "mod-adapter-core", "create-adapter", "client-port", "client-kernel",
            "client-app", "client-ui", "client-gpu",
        ):
            self.assertTrue(any(item["module"].startswith(f"{crate}/src/")
                                for item in report["modules"]), crate)


if __name__ == "__main__":
    unittest.main()
