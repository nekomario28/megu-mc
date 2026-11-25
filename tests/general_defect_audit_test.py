#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Fixture-driven tests for tools/general_defect_audit.py."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
AUDIT_PATH = REPO_ROOT / "tools" / "general_defect_audit.py"
SPEC = importlib.util.spec_from_file_location("general_defect_audit", AUDIT_PATH)
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)

DEV_NULL = "/dev/" + "null"
BAD_HOME = "/home/" + "mallory"
SHA40 = "a" * 40


class FixtureCase(unittest.TestCase):
    def audit_files(self, files):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for relative, text in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        return AUDIT.audit(root)

    def guard_rows(self, report, guard):
        return [row for row in report["findings"] if row["guard"] == guard]

    def assert_guard_fails(self, report, guard):
        self.assertTrue(
            self.guard_rows(report, guard),
            f"{guard} did not report the defective fixture: {report}",
        )

    def assert_guard_passes(self, report, guard):
        self.assertEqual(
            [],
            self.guard_rows(report, guard),
            f"{guard} reported a clean fixture: {report}",
        )


class PortabilityGuardTest(FixtureCase):
    def test_unguarded_dev_null_open_fails(self):
        report = self.audit_files({
            "tests/cross_os_test.py": f'with open("{DEV_NULL}") as stream:\n    pass\n',
        })
        self.assert_guard_fails(report, "cross-os-portability")

    def test_platform_guarded_dev_null_open_passes(self):
        report = self.audit_files({
            "tests/cross_os_test.py": (
                "import sys\n"
                f'if sys.platform != "win32":\n    stream = open("{DEV_NULL}")\n'
            ),
        })
        self.assert_guard_passes(report, "cross-os-portability")

    def test_unguarded_rust_dev_null_open_fails(self):
        report = self.audit_files({
            "src/demo.rs": f'use std::fs::File;\nfn probe() {{ let _ = File::open("{DEV_NULL}"); }}\n',
        })
        self.assert_guard_fails(report, "cross-os-portability")

    def test_dev_null_inside_python_comment_passes(self):
        report = self.audit_files({
            "tests/cross_os_test.py": f'# open("{DEV_NULL}") in documentation only\nprint("ok")\n',
        })
        self.assert_guard_passes(report, "cross-os-portability")


class PersonalPathGuardTest(FixtureCase):
    def test_non_synthetic_home_path_fails(self):
        report = self.audit_files({"notes.txt": f"fixture at {BAD_HOME}/project\n"})
        self.assert_guard_fails(report, "personal-path-hygiene")

    def test_documented_synthetic_home_path_passes(self):
        report = self.audit_files({"notes.txt": "fixture at /home/alice/project\n"})
        self.assert_guard_passes(report, "personal-path-hygiene")

    def test_macos_and_windows_non_synthetic_home_paths_fail(self):
        report = self.audit_files({
            "notes.txt": "mac " + "/Users/" + "mallory/project\nwin C:\\Users\\" + "mallory\\project\n",
        })
        rows = self.guard_rows(report, "personal-path-hygiene")
        self.assertEqual(2, len(rows), report)


class WorkflowSupplyChainGuardTest(FixtureCase):
    def test_tag_pinned_action_and_checkout_credentials_fail(self):
        report = self.audit_files({
            ".github/workflows/ci.yml": (
                "steps:\n"
                "  - uses: actions/checkout@v4\n"
            ),
        })
        rows = self.guard_rows(report, "workflow-supply-chain")
        self.assertGreaterEqual(len(rows), 2, report)

    def test_sha_pinned_action_and_disabled_checkout_credentials_pass(self):
        report = self.audit_files({
            ".github/workflows/ci.yml": (
                "steps:\n"
                f"  - uses: actions/checkout@{SHA40}\n"
                "    with:\n"
                "      persist-credentials: false\n"
            ),
        })
        self.assert_guard_passes(report, "workflow-supply-chain")

    def test_uppercase_hex_sha_pin_passes(self):
        report = self.audit_files({
            ".github/workflows/ci.yml": (
                "steps:\n"
                f"  - uses: actions/checkout@{SHA40.upper()}\n"
                "    with:\n"
                "      persist-credentials: false\n"
            ),
        })
        self.assert_guard_passes(report, "workflow-supply-chain")

    def test_commented_out_action_is_ignored(self):
        report = self.audit_files({
            ".github/workflows/ci.yml": "#  - uses: actions/checkout@v4\nsteps: []\n",
        })
        self.assert_guard_passes(report, "workflow-supply-chain")


class UnsafeGuardTest(FixtureCase):
    def test_new_unsafe_outside_allowlist_fails(self):
        report = self.audit_files({
            "src/new_module.rs": "fn probe() {\n    unsafe { core::hint::spin_loop() }\n}\n",
        })
        self.assert_guard_fails(report, "unsafe-safety-ratchet")

    def test_allowlisted_unsafe_with_safety_comment_passes(self):
        report = self.audit_files({
            "server-files/src/landlock_sandbox.rs": (
                "fn probe() {\n"
                "    // SAFETY: synthetic fixture carries no live pointer or ABI contract.\n"
                "    unsafe { core::hint::spin_loop() }\n"
                "}\n"
            ),
        })
        self.assert_guard_passes(report, "unsafe-safety-ratchet")

    def test_allowlisted_unsafe_without_safety_comment_fails(self):
        report = self.audit_files({
            "server-files/src/landlock_sandbox.rs": "fn probe() {\n    unsafe { core::hint::spin_loop() }\n}\n",
        })
        self.assert_guard_fails(report, "unsafe-safety-ratchet")

    def test_unsafe_text_inside_string_literal_passes(self):
        report = self.audit_files({
            "src/string_only.rs": 'fn probe() {\n    let text = "unsafe { not code }";\n}\n',
        })
        self.assert_guard_passes(report, "unsafe-safety-ratchet")

    def test_undocumented_unsafe_ratchet_in_baseline_file_fails(self):
        body = "".join("    unsafe { core::hint::spin_loop() }\n" for _ in range(16))
        report = self.audit_files({
            "core/src/simd_voxel.rs": "fn probe() {\n" + body + "}\n",
        })
        rows = self.guard_rows(report, "unsafe-safety-ratchet")
        self.assertTrue(any("undocumented unsafe count" in row["message"] for row in rows), report)

    def test_pinned_quantity_of_undocumented_unsafe_passes(self):
        body = "".join("    unsafe { core::hint::spin_loop() }\n" for _ in range(15))
        report = self.audit_files({
            "core/src/simd_voxel.rs": "fn probe() {\n" + body + "}\n",
        })
        self.assert_guard_passes(report, "unsafe-safety-ratchet")


class HttpCrlfGuardTest(FixtureCase):
    REQUEST = 'let request = format!("POST {path} HTTP/1.1\\r\\nHost: {host}\\r\\n\\r\\n");\n'

    def test_raw_request_without_crlf_validation_fails(self):
        report = self.audit_files({"client-app/src/demo.rs": self.REQUEST})
        self.assert_guard_fails(report, "http-crlf-injection")

    def test_empty_format_placeholder_request_fails(self):
        report = self.audit_files({
            "client-app/src/demo.rs": 'let request = format!("POST {} HTTP/1.1\\r\\nHost: {}\\r\\n\\r\\n");\n',
        })
        self.assert_guard_fails(report, "http-crlf-injection")

    def test_raw_request_with_explicit_validation_passes(self):
        report = self.audit_files({
            "client-app/src/demo.rs": (
                "fn validate_path(path: &str) {\n"
                "    assert!(!path.contains('\\r') && !path.contains('\\n'));\n"
                "}\n"
                + self.REQUEST
            ),
        })
        self.assert_guard_passes(report, "http-crlf-injection")


class DecompressionCapGuardTest(FixtureCase):
    LOOP = (
        "fn read(entries: &[&[u8]]) {\n"
        "    let mut out = Vec::new();\n"
        "    for entry in entries {\n"
        "        let decoded = decompress(entry);\n"
        "        out.push(decoded);\n"
        "    }\n"
        "}\n"
    )

    def test_multi_entry_decompression_without_aggregate_cap_fails(self):
        report = self.audit_files({"core/src/archive.rs": self.LOOP})
        self.assert_guard_fails(report, "archive-decompression-cap")

    def test_multi_entry_decompression_with_aggregate_cap_passes(self):
        report = self.audit_files({
            "core/src/archive.rs": "const MAX_TOTAL_BYTES: usize = 1024;\n" + self.LOOP,
        })
        self.assert_guard_passes(report, "archive-decompression-cap")


class RemoveZeroAndCodecGuardTest(FixtureCase):
    def test_ninth_vec_remove_zero_fails(self):
        body = "".join("    values.remove(0);\n" for _ in range(9))
        report = self.audit_files({"src/queue.rs": "fn drain(mut values: Vec<i32>) {\n" + body + "}\n"})
        self.assert_guard_fails(report, "hotloop-remove-zero-codec-ratchet")

    def test_baseline_quantity_of_vec_remove_zero_passes(self):
        body = "".join("    values.remove(0);\n" for _ in range(8))
        report = self.audit_files({"src/queue.rs": "fn drain(mut values: Vec<i32>) {\n" + body + "}\n"})
        self.assert_guard_passes(report, "hotloop-remove-zero-codec-ratchet")

    def test_remove_zero_inside_comment_does_not_count(self):
        body = "".join("    values.remove(0);\n" for _ in range(8))
        report = self.audit_files({
            "src/queue.rs": "// values.remove(0); in a comment\nfn drain(mut values: Vec<i32>) {\n" + body + "}\n",
        })
        self.assert_guard_passes(report, "hotloop-remove-zero-codec-ratchet")

    def test_new_lz4_codec_implementation_fails(self):
        report = self.audit_files({
            "core/src/demo_codec.rs": "fn lz4_decompress(input: &[u8]) -> Vec<u8> {\n    input.to_vec()\n}\n",
        })
        self.assert_guard_fails(report, "hotloop-remove-zero-codec-ratchet")

    def test_no_lz4_codec_implementation_passes(self):
        report = self.audit_files({
            "core/src/demo_codec.rs": "fn ordinary_helper(input: &[u8]) -> usize {\n    input.len()\n}\n",
        })
        self.assert_guard_passes(report, "hotloop-remove-zero-codec-ratchet")


class RenderLoopAllocationGuardTest(FixtureCase):
    def test_collection_allocation_inside_window_loop_fails(self):
        report = self.audit_files({
            "client-gpu/src/window.rs": (
                "fn frame(items: &[i32]) {\n"
                "    for item in items {\n"
                "        let mut seen = std::collections::HashSet::new();\n"
                "        seen.insert(item);\n"
                "    }\n"
                "}\n"
            ),
        })
        self.assert_guard_fails(report, "render-loop-allocation-ratchet")

    def test_collection_allocation_outside_window_loop_passes(self):
        report = self.audit_files({
            "client-gpu/src/window.rs": (
                "fn frame(items: &[i32]) {\n"
                "    let mut seen = std::collections::HashSet::new();\n"
                "    for item in items {\n"
                "        seen.insert(item);\n"
                "    }\n"
                "}\n"
            ),
        })
        self.assert_guard_passes(report, "render-loop-allocation-ratchet")


class UnboundedGlobalRegistryGuardTest(FixtureCase):
    GLOBAL = "static SESSIONS: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();\n"

    def test_unbounded_static_hash_map_fails(self):
        report = self.audit_files({"src/session.rs": self.GLOBAL})
        self.assert_guard_fails(report, "unbounded-global-registry")

    def test_static_hash_map_with_nearby_max_cap_passes(self):
        report = self.audit_files({"src/session.rs": "const MAX_SESSIONS: usize = 64;\n" + self.GLOBAL})
        self.assert_guard_passes(report, "unbounded-global-registry")

    def test_lowercase_static_hash_map_fails(self):
        report = self.audit_files({
            "src/session.rs": "static sessions: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();\n",
        })
        self.assert_guard_fails(report, "unbounded-global-registry")


class UnwiredPublicModuleGuardTest(FixtureCase):
    def test_public_module_without_production_reference_fails(self):
        report = self.audit_files({
            "core/src/lib.rs": "pub mod orphan;\n",
            "core/src/orphan.rs": "pub struct Thing;\n",
        })
        self.assert_guard_fails(report, "unwired-public-module")

    def test_public_module_with_production_reference_passes(self):
        report = self.audit_files({
            "core/src/lib.rs": "pub mod orphan;\n",
            "core/src/orphan.rs": "pub struct Thing;\n",
            "core/src/consumer.rs": "use crate::orphan::Thing;\npub fn use_it(input: Thing) -> Thing { input }\n",
        })
        self.assert_guard_passes(report, "unwired-public-module")

    def test_direct_module_import_is_a_production_reference(self):
        report = self.audit_files({
            "core/src/lib.rs": "pub mod orphan;\n",
            "core/src/orphan.rs": "pub struct Thing;\n",
            "core/src/consumer.rs": "use crate::orphan;\npub fn use_it() {}\n",
        })
        self.assert_guard_passes(report, "unwired-public-module")

    def test_cfg_test_only_reference_still_fails(self):
        report = self.audit_files({
            "core/src/lib.rs": "pub mod orphan;\n",
            "core/src/orphan.rs": "pub struct Thing;\n",
            "core/src/consumer.rs": "#[cfg(test)]\nmod tests {\n    use crate::orphan::Thing;\n}\n",
        })
        self.assert_guard_fails(report, "unwired-public-module")


class RepositoryAndCliTest(FixtureCase):
    def test_current_repository_is_clean_under_strict_audit(self):
        report = AUDIT.audit(REPO_ROOT)
        self.assertTrue(report["ok"], json.dumps(report["findings"][:20], indent=2))
        self.assertEqual(set(row["guard"] for row in report["findings"]), set())

    def test_json_strict_cli_passes_on_repository(self):
        completed = subprocess.run(
            [sys.executable, str(AUDIT_PATH), "--strict", "--json", "--root", str(REPO_ROOT)],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout[:1000])
        report = json.loads(completed.stdout)
        self.assertTrue(report["ok"])
        self.assertEqual(len(report["summary"]), 10)


if __name__ == "__main__":
    unittest.main()
