#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Unit tests for tools/docs_sync_audit.py (megu-docs-sync-audit/1).

Every check is driven twice over synthetic fixture trees: once against the
defect class the check exists to catch (an undocumented gate id, the stale
`5/10 PASS` scorecard fraction, a missing `L3` tier, an undocumented Cargo
feature) and once against the synced documentation. A check that cannot
fail, or that still fails on synced docs, is decoration. Fail-closed input
handling (exit 2, never a silent pass) is pinned separately, and one test
runs the audit against the real repository so the A14/A16 documents can
never drift from the parity tool or the root manifest without a red test.
"""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
TOOL = REPO / "tools" / "docs_sync_audit.py"

spec = importlib.util.spec_from_file_location("docs_sync_audit", TOOL)
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)

GATES = ("gate1_fishing", "gate2_enchanting", "gate3_branch_mining")
FEATURES = ("native-oracle", "java-compat", "mod-suite")


def tool_text(gates=GATES):
    return (
        "#!/usr/bin/env python3\n"
        "# SPDX-License-Identifier: LGPL-3.0-or-later\n"
        + "".join("def %s(texts):\n    return None\n\n" % g for g in gates)
        + "# prose mentioning gate9_fake must never count as a definition\n"
    )


def checklist_text(gates=GATES, score="3/3 PASS"):
    rows = "\n".join("| `%s` | row |" % g for g in gates)
    return "# Vanilla parity checklist\n\n%s\n\n## Scorecard\n\n%s\n" % (
        rows, score)


def compat_text(tiers=(0, 1, 2, 3, 4, 5), features=FEATURES):
    tier = " ".join("`L%d`" % t for t in tiers)
    feat = " ".join(features)
    return "# Mod compatibility\n\ntiers: %s\n\nfeatures: %s\n" % (tier, feat)


def cargo_text(features=FEATURES):
    lines = ["[package]", 'name = "fixture"', 'version = "0.0.0"', "[features]"]
    for feature in features:
        if feature == "mod-suite":
            lines += ["mod-suite = [", '    "dep:mod-adapter-core",', "]"]
        else:
            lines.append('%s = []' % feature)
    return "\n".join(lines) + "\n"


class FixtureTree(unittest.TestCase):
    """A hermetic repo skeleton the audit can run against."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def populate(self):
        self.write("tools/strict_vanilla_parity_audit.py", tool_text())
        self.write("docs/VANILLA_PARITY_CHECKLIST.md", checklist_text())
        self.write("docs/mod-compatibility.md", compat_text())
        self.write("Cargo.toml", cargo_text())

    def run_audit(self, *args):
        return subprocess.run(
            [sys.executable, str(TOOL), "--root", str(self.root)] + list(args),
            capture_output=True, text=True)

    def report(self, *args):
        result = self.run_audit("--json", *args)
        self.assertEqual(result.returncode, 0,
                         result.stdout + result.stderr)
        return json.loads(result.stdout)

    def check_by_id(self, report):
        return {check["id"]: check for check in report["checks"]}


class ParseHelpersTest(unittest.TestCase):
    def test_gate_ids_come_from_defs_only(self):
        ids = audit.extract_gate_ids(tool_text())
        self.assertEqual(ids, list(GATES))

    def test_no_gate_defs_is_an_error(self):
        with self.assertRaises(audit.AuditError):
            audit.extract_gate_ids("#!/usr/bin/env python3\n")

    def test_cargo_features_parse_multiline_arrays(self):
        self.assertEqual(audit.parse_cargo_features(cargo_text()),
                         list(FEATURES))

    def test_missing_features_section_is_an_error(self):
        with self.assertRaises(audit.AuditError):
            audit.parse_cargo_features("[package]\nname = 'x'\n")


class InputFailureTest(FixtureTree):
    def assert_exit2(self):
        result = self.run_audit("--strict")
        self.assertEqual(result.returncode, 2,
                         result.stdout + result.stderr)

    def test_missing_parity_tool(self):
        self.populate()
        (self.root / "tools/strict_vanilla_parity_audit.py").unlink()
        self.assert_exit2()

    def test_missing_checklist(self):
        self.populate()
        (self.root / "docs/VANILLA_PARITY_CHECKLIST.md").unlink()
        self.assert_exit2()

    def test_missing_compat_doc(self):
        self.populate()
        (self.root / "docs/mod-compatibility.md").unlink()
        self.assert_exit2()

    def test_missing_root_manifest(self):
        self.populate()
        (self.root / "Cargo.toml").unlink()
        self.assert_exit2()

    def test_missing_features_section(self):
        self.populate()
        self.write("Cargo.toml", '[package]\nname = "fixture"\n')
        self.assert_exit2()


class SyncCheckTest(FixtureTree):
    def test_synced_fixture_passes(self):
        self.populate()
        report = self.report("--strict")
        self.assertEqual(report["schema"], "megu-docs-sync-audit/1")
        self.assertEqual(report["summary"], {"pass": 3, "fail": 0, "total": 3})

    def test_default_mode_is_scorecard_even_with_violations(self):
        self.populate()
        self.write("docs/mod-compatibility.md",
                   compat_text(tiers=(0, 1, 2, 4, 5)))
        result = self.run_audit()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FAIL", result.stdout)

    def test_undocumented_gate_fails(self):
        self.populate()
        self.write("docs/VANILLA_PARITY_CHECKLIST.md",
                   checklist_text(gates=("gate1_fishing", "gate3_branch_mining")))
        report = self.report()
        check = self.check_by_id(report)["parity_gates_documented"]
        self.assertEqual(check["status"], "FAIL")
        self.assertTrue(any("gate2_enchanting" in f for f in check["findings"]))
        self.assertEqual(self.run_audit("--strict").returncode, 1)

    def test_stale_5_of_10_fraction_fails(self):
        self.populate()
        self.write("docs/VANILLA_PARITY_CHECKLIST.md",
                   checklist_text(score="5/10 PASS"))
        report = self.report()
        check = self.check_by_id(report)["parity_gates_documented"]
        self.assertEqual(check["status"], "FAIL")
        self.assertTrue(any("5/10" in f for f in check["findings"]))
        self.assertEqual(self.run_audit("--strict").returncode, 1)

    def test_fraction_not_adjacent_to_pass_is_not_a_scorecard(self):
        self.populate()
        self.write("docs/VANILLA_PARITY_CHECKLIST.md",
                   checklist_text(score="history: 5/10 in an old snapshot"))
        self.assertEqual(self.report("--strict")["summary"]["fail"], 0)

    def test_missing_tier_fails(self):
        self.populate()
        self.write("docs/mod-compatibility.md",
                   compat_text(tiers=(0, 1, 2, 4, 5)))
        report = self.report()
        check = self.check_by_id(report)["compat_tiers_defined"]
        self.assertEqual(check["status"], "FAIL")
        self.assertTrue(any("L3" in f for f in check["findings"]))
        self.assertEqual(self.run_audit("--strict").returncode, 1)

    def test_undocumented_feature_fails(self):
        self.populate()
        self.write("docs/mod-compatibility.md",
                   compat_text(features=("native-oracle", "mod-suite")))
        report = self.report()
        check = self.check_by_id(report)["cargo_features_documented"]
        self.assertEqual(check["status"], "FAIL")
        self.assertTrue(any("java-compat" in f for f in check["findings"]))
        self.assertEqual(self.run_audit("--strict").returncode, 1)

    def test_empty_features_section_passes(self):
        self.populate()
        self.write("Cargo.toml", '[package]\nname = "fixture"\n[features]\n')
        report = self.report("--strict")
        check = self.check_by_id(report)["cargo_features_documented"]
        self.assertEqual(check["status"], "PASS")

    def test_json_report_shape(self):
        self.populate()
        report = self.report()
        self.assertEqual(report["schema"], "megu-docs-sync-audit/1")
        self.assertEqual(
            [c["id"] for c in report["checks"]],
            ["parity_gates_documented", "compat_tiers_defined",
             "cargo_features_documented"])
        self.assertEqual(report["summary"]["total"], 3)


class RealRepoTest(unittest.TestCase):
    def test_real_repo_is_synced(self):
        """The live A14/A16 docs must match the parity tool and Cargo.toml."""
        result = subprocess.run(
            [sys.executable, str(TOOL), "--root", str(REPO), "--strict"],
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 0,
                         result.stdout + result.stderr)
        report = json.loads(subprocess.run(
            [sys.executable, str(TOOL), "--root", str(REPO), "--json"],
            capture_output=True, text=True).stdout)
        self.assertEqual(report["summary"]["fail"], 0)
        ids = audit.extract_gate_ids(
            (REPO / "tools/strict_vanilla_parity_audit.py").read_text())
        self.assertEqual(len(ids), 11)
        features = audit.parse_cargo_features(
            (REPO / "Cargo.toml").read_text())
        for feature in ("java-compat", "fabric-probe", "neoforge-events",
                        "create-offload", "mod-suite", "native-oracle"):
            self.assertIn(feature, features)


if __name__ == "__main__":
    unittest.main()
