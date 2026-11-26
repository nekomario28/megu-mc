#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Production hotpath and real-world mod-tool smoke tests (no build)."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GUARD = ROOT / 'tools/playblocker_guard_audit.py'
MOD = ROOT / 'tools/megu_mod_dev.py'


def run(script, *args):
    return subprocess.run([sys.executable, str(script), *map(str, args)],
                          cwd=ROOT, capture_output=True, text=True, check=False)


class PlayblockerGuardTests(unittest.TestCase):
    def test_clean_checkout(self):
        result = run(GUARD)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_production_crashes_and_test_mask(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            path = root / 'proto/src/seamless_portal.rs'
            original = path.read_text(encoding='utf-8')
            for snippet in ('todo!()', 'value.unwrap()'):
                with self.subTest(snippet=snippet):
                    path.write_text(original + '\nfn injected() { ' + snippet + '; }\n', encoding='utf-8')
                    self.assertEqual(run(GUARD, '--root', root).returncode, 1)
            path.write_text(original + '\n#[cfg(all(test, feature = "x"))]\nmod injected { #[test] fn check() { value.unwrap(); } }\n', encoding='utf-8')
            result = run(GUARD, '--root', root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_conflicting_magic(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            path = root / 'proto/src/subworld_wire.rs'
            text = path.read_text(encoding='utf-8')
            path.write_text(text.replace('SW6D_MAGIC: [u8; 4] = *b"SW6D"',
                                         'SW6D_MAGIC: [u8; 4] = *b"WRNG"'), encoding='utf-8')
            result = run(GUARD, '--root', root)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn('SW6D', result.stdout)
            portal = root / 'proto/src/seamless_portal.rs'
            portal.write_text(portal.read_text(encoding='utf-8') +
                              '\nconst OTHER_MAGIC: [u8; 4] = *b"MSP1";\n', encoding='utf-8')
            result = run(GUARD, '--root', root)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn('duplicate MSP1', result.stdout)

    @staticmethod
    def copy_audit_inputs(root):
        sys.path.insert(0, str(ROOT / 'tools'))
        from playblocker_guard_audit import HOTPATHS, WIRES, BASELINE
        extra = {'proto/src/subworld_wire.rs', 'proto/src/mod_wire.rs'}
        for name in set(HOTPATHS) | {item for wire in WIRES for item in wire[2:]} | {BASELINE} | extra:
            dest = root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, dest)

    def test_japanese_spaces_and_corrupt_configs(self):
        with tempfile.TemporaryDirectory() as tmp:
            mod_dir = Path(tmp) / 'マイクラ テスト' / 'mod_one'
            init = run(MOD, 'init', 'mod_one', '--out', mod_dir)
            self.assertEqual(init.returncode, 0, init.stdout + init.stderr)
            validate = run(MOD, 'validate', mod_dir, '--json')
            self.assertEqual(validate.returncode, 0, validate.stdout + validate.stderr)
            packed = run(MOD, 'pack', mod_dir, '--out', mod_dir / 'dist' / 'mod_one.mmod')
            self.assertEqual(packed.returncode, 0, packed.stdout + packed.stderr)
            self.assertTrue((mod_dir / 'dist/mod_one.mmod').is_file())
            for filename, corruption in (('megu-mod-bundle.json', '{"schema":'),
                                         ('megu-mod.toml', '[mod\n')):
                with self.subTest(filename=filename):
                    path = mod_dir / filename
                    original = path.read_bytes()
                    try:
                        path.write_text(corruption, encoding='utf-8')
                        result = run(MOD, 'validate', mod_dir, '--json')
                        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                        self.assertNotIn('Traceback', result.stderr + result.stdout)
                        report = json.loads(result.stdout)
                        self.assertFalse(report['ok'])
                        self.assertTrue(report['diagnostics'])
                        self.assertTrue(all('code' in d and 'message' in d for d in report['diagnostics']))
                    finally:
                        path.write_bytes(original)


if __name__ == '__main__':
    unittest.main()
