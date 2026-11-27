#!/usr/bin/env python3
"""Release-gate coverage: verify.sh must keep up with the workspace it ships.

A crate or feature that lands without a verify.sh stage is a release gate that
cannot see it. The verify.sh EXIT handler must preserve generated build outputs
so the caller can inspect or reuse them. This test reads tests/verify.sh:

* every top-level directory carrying a Cargo.toml is tested
  (`cargo test --manifest-path <dir>/Cargo.toml`) and its build output is
  preserved by the verify.sh EXIT handler,
* every adapter feature declared in the root [features] table is exercised
  (`--features <name>` somewhere in the script), and
* every tests/*.py — not just *_test.py, so wire/oracle/live scripts cannot
  slip past ungated — is invoked or has an explicit documented exemption, and
* tools/provenance_check.py is invoked live and under --self-test.

Run: python3 tests/verify_gate_coverage_test.py
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERIFY = ROOT / "tests" / "verify.sh"

# The crates expected today; the live scan below also fails on an unexpected
# extra crate, so a new one cannot slip past by just not being named here.
EXPECTED_CRATES = (
    "client-port",
    "client-kernel",
    "client-app",
    "client-ui",
    "client-gpu",
    "mod-offload",
    "mod-adapter-core",
    "create-adapter",
    "proto",
    "state-store",
    "server-files",
    "core",
    "wasm-mod-runtime",
)
# Root Cargo.toml features the adapter lane must gate. `native-oracle` and
# `java-compat` are exempt: the first is a CI-only fixture switch, the second is
# pulled in transitively by fabric-probe/neoforge-events.
ADAPTER_FEATURES = ("fabric-probe", "neoforge-events", "create-offload")
UNGATED_FEATURES = frozenset({"native-oracle", "java-compat"})

# Standalone scripts with prerequisites that the portable verify.sh gate cannot
# provide. Keep reasons specific so exceptions remain auditable.
EXEMPT_TEST_FILES = {
    "lighting_parity_test.py": (
        "Requires the pinned vanilla 1.21.1 server JAR at a machine-local absolute "
        "path plus Java 21, which the portable verify.sh runner cannot provide."
    ),
    "block_shape_redstone_oracle.py": (
        "Live Protocol 767 oracle: requires a built megu binary and the pinned "
        "vanilla server.jar fixtures, which the portable verify.sh runner cannot provide."
    ),
    "survival_creative_oracle_matrix.py": (
        "Live differential matrix: requires a built megu binary and the pinned "
        "vanilla server.jar fixtures, which the portable verify.sh runner cannot provide."
    ),
    "trademark_hygiene_audit_test.py": (
        "Hermetic fixture tests for tools/trademark_hygiene_audit.py; invoked by "
        "tools/run_fast_gates.py in the CI fast-gate lane. tests/verify.sh is a "
        "shell script outside the Python-gate-test edit scope that added this test."
    ),
    "cfg_fork_audit_test.py": (
        "Hermetic fixture tests for tools/cfg_fork_audit.py; invoked by "
        "tools/run_fast_gates.py in the CI fast-gate lane. tests/verify.sh is a "
        "shell script outside the Python-gate-test edit scope that added this test."
    ),
    "lock_order_audit_test.py": (
        "Hermetic fixture tests for tools/lock_order_audit.py; invoked by "
        "tools/run_fast_gates.py in the CI fast-gate lane. tests/verify.sh is a "
        "shell script outside the Python-gate-test edit scope that added this test."
    ),
}

_FEATURES_TABLE = re.compile(r"(?ms)^\[features\]\s*\n(.*?)(?=^\[|\Z)")
_FEATURE_KEY = re.compile(r"(?m)^([A-Za-z0-9_-]+)\s*=")
# Any python3 invocation of a script directly under tests/. Matching only
# *_test.py let wire/oracle/live scripts (protocol767.py, *_wire.py,
# *_oracle.py, gate_scripts.py, ...) drift out of verify.sh unnoticed.
_PYTHON_TEST_COMMAND = re.compile(
    r"(?m)^[ \t]*(?:timeout[ \t]+\d+[ \t]+)?python3[ \t]+"
    r"tests/([A-Za-z0-9_]+\.py)(?=[ \t]|$)"
)


def verify_script():
    return VERIFY.read_text()


def root_crates():
    return sorted(path.parent.name for path in ROOT.glob("*/Cargo.toml"))


def declared_features():
    text = (ROOT / "Cargo.toml").read_text()
    table = _FEATURES_TABLE.search(text)
    assert table is not None, "root Cargo.toml has no [features] table"
    return set(_FEATURE_KEY.findall(table.group(1)))


def artifact_report_body(script):
    match = re.search(r"(?ms)^report_verify_artifacts\(\)\s*\{(.*?)^\}", script)
    assert match is not None, "verify.sh has no report_verify_artifacts function"
    return match.group(1)


class VerifyGateCoverage(unittest.TestCase):
    def test_every_python_test_is_invoked_or_documented_as_exempt(self):
        tests = {path.name for path in (ROOT / "tests").glob("*.py")}
        gated = set(_PYTHON_TEST_COMMAND.findall(verify_script()))
        self.assertFalse(set(EXEMPT_TEST_FILES) - tests,
                         "EXEMPT_TEST_FILES includes a file that no longer exists")
        for filename, reason in EXEMPT_TEST_FILES.items():
            with self.subTest(exempt=filename):
                self.assertTrue(reason.strip(), f"{filename} needs an exemption reason")
                self.assertNotIn(filename, gated,
                                 f"{filename} is gated and should not be exempt")
        for filename in sorted(tests):
            with self.subTest(test=filename):
                self.assertTrue(
                    filename in gated or filename in EXEMPT_TEST_FILES,
                    f"tests/{filename} is neither run in verify.sh nor explicitly exempt",
                )

    def test_workspace_crate_set_is_exactly_the_expected_one(self):
        self.assertEqual(root_crates(), sorted(EXPECTED_CRATES))

    def test_every_root_crate_is_tested_in_verify_sh(self):
        script = verify_script()
        for crate in root_crates():
            with self.subTest(crate=crate):
                self.assertRegex(
                    script,
                    rf"(?m)^cargo test --manifest-path {re.escape(crate)}/Cargo\.toml\b",
                    f"verify.sh does not cargo-test {crate}")

    def test_exit_handler_preserves_artifacts_without_destructive_cleanup(self):
        script = verify_script()
        body = artifact_report_body(script)
        self.assertRegex(script, r"(?m)^trap report_verify_artifacts EXIT$")
        self.assertNotRegex(body, r"(?m)\b(?:rm|rmdir|unlink)\b")
        for crate in root_crates():
            with self.subTest(crate=crate):
                self.assertNotIn(f"{crate}/target", body,
                                 f"EXIT handler must preserve {crate}/target")
        self.assertIn("preserving build manifest", body)
        self.assertIn("preserving Cargo target directories", body)
        self.assertIn("target/companion-e2e", body)

    def test_every_adapter_feature_is_checked_in_verify_sh(self):
        script = verify_script()
        declared = declared_features()
        for feature in ADAPTER_FEATURES:
            with self.subTest(feature=feature):
                self.assertIn(feature, declared,
                              f"{feature} is not declared in root Cargo.toml [features]")
                self.assertIn(f"--features {feature}", script,
                              f"verify.sh does not gate --features {feature}")

    def test_no_undeclared_adapter_feature_slips_past(self):
        script = verify_script()
        for feature in sorted(declared_features() - UNGATED_FEATURES):
            with self.subTest(feature=feature):
                self.assertIn(f"--features {feature}", script,
                              f"verify.sh does not gate declared feature {feature}")

    def test_provenance_check_runs_live_and_in_self_test(self):
        script = verify_script()
        self.assertRegex(script, r"(?m)^python3 tools/provenance_check\.py\s*$",
                         "verify.sh does not run tools/provenance_check.py")
        self.assertRegex(script, r"(?m)^python3 tools/provenance_check\.py --self-test\s*$",
                         "verify.sh does not run tools/provenance_check.py --self-test")


if __name__ == "__main__":
    unittest.main()
