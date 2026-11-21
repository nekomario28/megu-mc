#!/usr/bin/env python3
"""Release-gate coverage: verify.sh must keep up with the workspace it ships.

A crate or feature that lands without a verify.sh stage is a release gate that
cannot see it, and a crate whose target the EXIT trap does not remove can seed
the next run's build. This test reads tests/verify.sh against the live tree:

* every top-level directory carrying a Cargo.toml is both tested
  (`cargo test --manifest-path <dir>/Cargo.toml`) and cleaned in
  `cleanup_verify_artifacts` (`<dir>/target` on the rm -rf line),
* every adapter feature declared in the root [features] table is exercised
  (`--features <name>` somewhere in the script), and
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
    "mod-offload",
    "mod-adapter-core",
    "create-adapter",
    "proto",
    "state-store",
)
# Root Cargo.toml features the adapter lane must gate. `native-oracle` and
# `java-compat` are exempt: the first is a CI-only fixture switch, the second is
# pulled in transitively by fabric-probe/neoforge-events.
ADAPTER_FEATURES = ("fabric-probe", "neoforge-events", "create-offload")
UNGATED_FEATURES = frozenset({"native-oracle", "java-compat"})

_FEATURES_TABLE = re.compile(r"(?ms)^\[features\]\s*\n(.*?)(?=^\[|\Z)")
_FEATURE_KEY = re.compile(r"(?m)^([A-Za-z0-9_-]+)\s*=")


def verify_script():
    return VERIFY.read_text()


def root_crates():
    return sorted(path.parent.name for path in ROOT.glob("*/Cargo.toml"))


def declared_features():
    text = (ROOT / "Cargo.toml").read_text()
    table = _FEATURES_TABLE.search(text)
    assert table is not None, "root Cargo.toml has no [features] table"
    return set(_FEATURE_KEY.findall(table.group(1)))


def cleanup_body(script):
    match = re.search(r"(?ms)^cleanup_verify_artifacts\(\)\s*\{(.*?)^\}", script)
    assert match is not None, "verify.sh has no cleanup_verify_artifacts function"
    return match.group(1)


class VerifyGateCoverage(unittest.TestCase):
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

    def test_every_root_crate_target_is_cleaned_on_exit(self):
        body = cleanup_body(verify_script())
        self.assertIn("rm -rf", body)
        for crate in root_crates():
            with self.subTest(crate=crate):
                self.assertIn(f"{crate}/target", body,
                              f"cleanup_verify_artifacts does not remove {crate}/target")

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
