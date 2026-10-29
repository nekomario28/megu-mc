#!/usr/bin/env python3
"""A coverage report must see real 1.21.1 input and inline reference roles."""
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import feature_type_coverage as coverage


class FeatureCoverageTests(unittest.TestCase):
    def test_native_directory_and_inline_configured_references_are_counted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data = {
                'configured_feature/root.json': {'type': 'minecraft:simple_random_selector',
                    'config': {'features': [
                        {'feature': {'type': 'minecraft:sculk_patch', 'config': {}}, 'placement': []},
                        {'feature': 'minecraft:mushroom', 'placement': []}]}},
                'configured_feature/mushroom.json': {'type': 'minecraft:fossil', 'config': {}},
                'placed_feature/top.json': {'feature': 'minecraft:root', 'placement': []},
                'biome/test.json': {'features': [['minecraft:top']]},
            }
            for name, value in data.items():
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(json.dumps(value))
            output = io.StringIO()
            # Pin the supported set for this graph/filter test. Real parser-arm
            # extraction is tested below; adding a feature must not silently stop
            # this fixture exercising inline and named missing-type reporting.
            with patch.object(coverage, 'DATA', root), \
                 patch.object(coverage, 'implemented_arms', return_value={'simple_random_selector'}), \
                 patch.object(sys, 'argv', ['coverage', '--unimplemented']), \
                 contextlib.redirect_stdout(output):
                self.assertEqual(coverage.main(), 0)
            text = output.getvalue()
            self.assertIn('2 files + 1 inline, 3 types', text)
            self.assertRegex(text, r'sculk_patch\s+1\s+1\s+1')
            # Both the inline and named types absent from the supplied supported
            # set must be named, regardless of the current port's feature set.
            self.assertRegex(text, r'fossil\s+1\s+1\s+1')

    def test_missing_empty_and_invalid_input_cannot_report_zero_gaps(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(coverage, 'DATA', Path(tmp)):
            with self.assertRaisesRegex(ValueError, 'missing'):
                coverage.load_dir('configured_feature')
            folder = Path(tmp) / 'configured_feature'
            folder.mkdir()
            with self.assertRaisesRegex(ValueError, 'empty'):
                coverage.load_dir('configured_feature')
            (folder / 'broken.json').write_text('{')
            with self.assertRaises(json.JSONDecodeError):
                coverage.load_dir('configured_feature')

    def test_json_keys_inside_arms_are_not_implemented_types(self):
        arms = coverage.implemented_arms()
        self.assertTrue({'ore', 'tree', 'random_patch'}.issubset(arms))
        self.assertTrue({'size', 'targets', 'state_provider', 'config'}.isdisjoint(arms))


if __name__ == '__main__':
    unittest.main()
