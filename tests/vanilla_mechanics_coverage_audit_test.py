#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Regression tests for registry discovery in the mechanics coverage audit."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools/vanilla_mechanics_coverage_audit.py"
spec = importlib.util.spec_from_file_location("vanilla_mechanics_coverage_audit", SCRIPT)
audit_tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit_tool)


class DamageTypeRegistryDiscoveryTest(unittest.TestCase):
    def write_fixture(self, root):
        src = root / "src"
        src.mkdir()
        (src / "damage.rs").write_text('''
const VANILLA_DAMAGE_TYPES: &[&str] = &["arrow", "fall", "generic"];
fn damage_type_known(name: &str) -> bool {
    VANILLA_DAMAGE_TYPES.contains(&name)
}
fn dyn_damage_type_usable(scaling: &str) -> bool {
    const SCALING: &[&str] = &["never", "always", "when_caused_by_living_non_player"];
    SCALING.contains(&scaling)
}
''')

    def test_scaling_validator_is_not_the_damage_type_registry(self):
        # Regression guard: a `damage_type` codec validator's DamageScaling
        # enum table was previously mistaken for the damage-type registry.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write_fixture(root)
            sources = audit_tool.load_sources(root)
            self.assertEqual(
                audit_tool.damage_type_registry(sources),
                ["arrow", "fall", "generic"],
            )

    def test_repository_registry_contains_no_scaling_values(self):
        sources = audit_tool.load_sources(ROOT)
        registry = set(audit_tool.damage_type_registry(sources))
        self.assertNotIn("always", registry)
        self.assertNotIn("never", registry)
        self.assertNotIn("when_caused_by_living_non_player", registry)
        self.assertIn("arrow", registry)
        self.assertIn("fall", registry)


if __name__ == "__main__":
    unittest.main()
