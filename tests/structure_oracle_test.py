#!/usr/bin/env python3
"""Check pinned structure assets and native-to-wire transform identity mapping."""
import hashlib
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
from structure_transform_oracle import encode
from vanilla_java import JAR_SHA1
from vanilla_structure_assets import export, PREFIXES


class StructureOracleTests(unittest.TestCase):
    def test_every_exported_member_matches_its_authenticated_manifest(self):
        root = ROOT / 'datapacks/vanilla'
        manifest = json.loads((root / 'structure-provenance.json').read_text())
        self.assertEqual(manifest['server_sha1'], JAR_SHA1)
        self.assertEqual(manifest['minecraft'], '1.21.1')
        installed = {p.relative_to(root).as_posix()
                     for prefix, (suffix, _) in PREFIXES.items()
                     for p in (root / prefix).rglob('*' + suffix)}
        self.assertEqual(installed, set(manifest['members_sha256']))
        self.assertTrue(installed)
        for name, expected in manifest['members_sha256'].items():
            self.assertEqual(hashlib.sha256((root / name).read_bytes()).hexdigest(), expected, name)

    def test_wrong_jar_cannot_create_or_replace_structure_assets(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            jar, output = root / 'server.jar', root / 'output'
            jar.write_bytes(b'not the pinned executable')
            with self.assertRaisesRegex(ValueError, 'not the pinned'):
                export(jar, output, True)
            self.assertFalse(output.exists())

    def test_native_order_is_remapped_by_state_identity(self):
        blocks = [{'name': 'test', 'minStateId': 0, 'maxStateId': 1,
                   'states': [{'name': 'active', 'values': ['false', 'true']}]}]
        row = lambda state, indices: 'Block{minecraft:test}[active=' + state + ']\t' + '\t'.join(map(str, indices))
        lines = [row('true', [0] + [1] * 11), row('false', [1] + [0] * 11)]
        actual = encode('\n'.join(lines), blocks)
        self.assertEqual(actual, struct.pack('>24I', *([0] + [1] * 11 + [1] + [0] * 11)))
        for bad in [lines[:1], [lines[0], lines[0]],
                    [row('true', [-1] * 12), lines[1]]]:
            with self.assertRaises(ValueError):
                encode('\n'.join(bad), blocks)


if __name__ == '__main__':
    unittest.main()
