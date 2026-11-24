#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Synthetic regression tests for tools/license_spdx_audit.py.

Each case builds a minimal licence-clean tree in a temp root and proves the
gate passes it, then injects one regression (retired triple-licence header,
upstream-donor attribution marker, manifest licence drift, missing anchor)
and proves the gate fails with a path:line finding. The injected literals are
built from fragments so this file never carries the text the gate hunts for.
"""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "license_spdx_audit.py"

UNIFIED = "LGPL-3.0-or-later"
OBSOLETE = " OR ".join((UNIFIED, "GPL-3.0-or-later", "AGPL-3.0-or-later"))
DONOR = "Pump" + "kin"
SPDX_TAG = "SPDX-License-Identifier"


def run(*args):
    return subprocess.run([sys.executable, str(SCRIPT), *map(str, args)],
                          capture_output=True, text=True, check=False)


def clean_tree(root):
    """A minimal tree that must pass every check."""
    (root / "docs").mkdir(parents=True)
    for name in ("LICENSE", "docs/COPYING.GPL", "NOTICE", "THIRD_PARTY.md"):
        (root / name).write_text("licence anchor\n", encoding="utf-8")
    (root / "Cargo.toml").write_text(
        f'[package]\nname = "fixture"\nlicense = "{UNIFIED}"\n',
        encoding="utf-8")
    (root / "crate-a").mkdir()
    (root / "crate-a" / "Cargo.toml").write_text(
        f'[package]\nname = "crate-a"\nlicense = "{UNIFIED}"\n',
        encoding="utf-8")
    (root / "src").mkdir()
    (root / "src" / "main.rs").write_text(
        f"// {SPDX_TAG}: {UNIFIED}\nfn main() {{}}\n", encoding="utf-8")
    (root / "tool.py").write_text(
        f"# {SPDX_TAG}: {UNIFIED}\nprint('ok')\n", encoding="utf-8")


class LicenseSpdxAuditTests(unittest.TestCase):
    def test_live_repository_passes(self):
        result = run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_clean_fixture_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clean_tree(root)
            result = run("--root", root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_triple_spdx_header_fails(self):
        for name, header in (
            ("src/lib.rs", f"// {SPDX_TAG}: {OBSOLETE}"),
            ("tool.sh", f"# {SPDX_TAG}: {OBSOLETE}"),
            ("site.mjs", f"// {SPDX_TAG}: {OBSOLETE}"),
            ("shader.wgsl", f"// {SPDX_TAG}: {OBSOLETE}"),
        ):
            with self.subTest(file=name):
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    clean_tree(root)
                    target = root / name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(header + "\n", encoding="utf-8")
                    result = run("--root", root)
                    self.assertEqual(result.returncode, 1,
                                     f"{name} not caught: {result.stdout}{result.stderr}")
                    self.assertIn(f"{name}:1", result.stdout)

    def test_donor_attribution_marker_fails(self):
        for line in (
            f"// {DONOR}-derived internal-method vectors, retained as pins.",
            f"// {DONOR.lower()} xoroshiro128.rs test vectors",
            f"// {DONOR}'s `feature/tree` was read for the *semantics*.",
            f"// Algorithm referenced from vanilla/{DONOR}; code is independent.",
            f"// {DONOR.lower()} multi_noise_sampler tests: seed, quart pos",
        ):
            with self.subTest(marker=line):
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    clean_tree(root)
                    (root / "src" / "main.rs").write_text(
                        line + "\nfn main() {}\n", encoding="utf-8")
                    result = run("--root", root)
                    self.assertEqual(result.returncode, 1,
                                     f"marker not caught: {result.stdout}{result.stderr}")
                    self.assertIn("src/main.rs:1", result.stdout)

    def test_donor_block_names_and_comparisons_are_not_markers(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clean_tree(root)
            (root / "src" / "main.rs").write_text(
                "// snow_golem removes its pumpkin; carved_pumpkin is wearable.\n"
                f"// aim past what Storia and {DONOR} (a Rust reimplementation)\n"
                f"// do today; where {DONOR} serializes per recipient, this\n"
                "// cache encodes once per key.\n"
                "fn main() {}\n", encoding="utf-8")
            result = run("--root", root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_evidence_snapshots_keep_their_markers(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clean_tree(root)
            evidence = root / "evidence" / "snapshot.rs"
            evidence.parent.mkdir(parents=True)
            evidence.write_text(
                f"// {DONOR}-derived vectors kept as a frozen record.\n",
                encoding="utf-8")
            result = run("--root", root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_manifest_license_mismatch_fails(self):
        for declared in ("MIT", "GPL-3.0-or-later", OBSOLETE):
            with self.subTest(license=declared):
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    clean_tree(root)
                    (root / "crate-a" / "Cargo.toml").write_text(
                        f'[package]\nname = "crate-a"\nlicense = "{declared}"\n',
                        encoding="utf-8")
                    result = run("--root", root)
                    self.assertEqual(result.returncode, 1,
                                     f"{declared} not caught: {result.stdout}{result.stderr}")
                    self.assertIn("crate-a/Cargo.toml", result.stdout)

    def test_manifest_without_license_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clean_tree(root)
            (root / "crate-a" / "Cargo.toml").write_text(
                '[package]\nname = "crate-a"\n', encoding="utf-8")
            result = run("--root", root)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_missing_anchor_file_fails(self):
        for name in ("LICENSE", "docs/COPYING.GPL", "NOTICE", "THIRD_PARTY.md"):
            with self.subTest(anchor=name):
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    clean_tree(root)
                    (root / name).unlink()
                    result = run("--root", root)
                    self.assertEqual(result.returncode, 1,
                                     f"{name} removal not caught: "
                                     f"{result.stdout}{result.stderr}")
                    self.assertIn(name, result.stdout)

    def test_docs_may_narrate_the_old_expression(self):
        # Migration docs are .md, not source: the retired expression in prose
        # must not be a finding.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clean_tree(root)
            (root / "LICENSING.md").write_text(
                f"| old | `{OBSOLETE}` | unified 2026-11-22 |\n",
                encoding="utf-8")
            result = run("--root", root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_gate_is_registered(self):
        self.assertIn("tools/license_spdx_audit.py",
                      (ROOT / "tools" / "run_fast_gates.py").read_text())
        workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
        self.assertIn("python3 tools/license_spdx_audit.py", workflow)
        self.assertIn("python3 tests/license_spdx_audit_test.py", workflow)


if __name__ == "__main__":
    unittest.main()
