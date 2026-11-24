#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Pure-Python hermetic regression tests for the hardcode guard."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / 'tools/hardcode_guard_audit.py'
spec = importlib.util.spec_from_file_location('hardcode_guard', SCRIPT)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class HardcodeGuardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'src').mkdir()
        self.baseline({}, {})

    def baseline(self, counts, traps):
        file = self.root / 'audit/hardcode_guard_baseline.json'
        file.parent.mkdir(exist_ok=True)
        file.write_text(json.dumps({'counts': counts, 'namespace_traps': traps}))

    def source(self, path, contents):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(contents)

    def run_audit(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), '--root', str(self.root),
                               '--json', *args], capture_output=True, text=True)

    def check(self, code, rule=None):
        result = self.run_audit()
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        data = json.loads(result.stdout)
        if rule:
            self.assertIn(rule, [f['rule'] for f in data['findings']])
        return data

    def test_clean_generic_file(self):
        self.source('client-port/src/dynamic_light_resolver.rs', 'let name = "minecraft:stone";')
        self.check(0)

    def test_generic_mod_id_fails(self):
        self.source('mod-offload/src/engine.rs', 'let block = "botania:mana_pool";')
        findings = self.check(1, 'zero_mod_id_in_generic_engine')['findings']
        self.assertEqual(findings[0]['line'], 1)
        self.assertIn('botania:mana_pool', findings[0]['literal'])

    def test_comments_and_cfg_test_ignored(self):
        self.source('mod-offload/src/engine.rs', '''// "botania:mana_pool"
/* nested /* "create:shaft" */ */
#[cfg(test)] mod tests { fn fixture() { let x = "iceandfire:fire_dragon"; } }
let name = "minecraft:stone";
''')
        self.check(0)

    def test_compound_cfg_test_modules_and_production(self):
        path = 'src/blocks.rs'
        self.source(path, '''#[cfg(all(test, feature = "mod-suite"))]
mod t { fn x() { let _ = "create:shaft"; } }
#[cfg(all(feature = "mod-suite", test))]
mod other { fn x() { let _ = "botania:pool"; } }
''')
        self.assertEqual(self.check(0)['counts'], {})
        self.source(path, (self.root / path).read_text() + 'let _ = "create:shaft";\n')
        findings = self.check(1, 'mod_namespace_spread_ratchet')['findings']
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]['line'], 5)

    def test_compound_cfg_without_test_is_production(self):
        self.source('src/blocks.rs', '''#[cfg(all(feature = "mod-suite", feature = "extra"))]
mod production { fn x() { let _ = "create:shaft"; } }
''')
        findings = self.check(1, 'mod_namespace_spread_ratchet')['findings']
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]['line'], 2)

    def test_individual_test_function_ignored(self):
        self.source('src/blocks.rs', '''#[cfg(feature = "mod-suite")]
#[test]
fn test_block() { if true { let _ = "create:shaft"; } }
let _ = "botania:pool";
''')
        findings = self.check(1, 'mod_namespace_spread_ratchet')['findings']
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]['line'], 4)

    def test_raw_strings_and_nested_test_braces(self):
        self.source('client-port/src/builtin_shadow_wgsl.rs', '''#[cfg(test)]
mod tests { fn t() { if true { let v = r#"create:shaft"#; } } }
let value = r#"aether:skyroot"#;
''')
        self.check(1, 'zero_mod_id_in_generic_engine')

    def test_namespace_suffix_trap(self):
        self.source('src/blocks.rs', 'name.starts_with("minecraft:") && name.ends_with("_bed")')
        self.check(1, 'vanilla_only_namespace_suffix_trap')

    def test_format_forced_namespace(self):
        self.source('src/blocks.rs', 'format!("minecraft:{block}") == other')
        self.check(1, 'vanilla_only_namespace_suffix_trap')

    def test_comment_cannot_complete_expression(self):
        self.source('src/blocks.rs', 'name.starts_with("minecraft:") /* && name.ends_with("_bed") */')
        self.check(0)

    def test_spread_ratchet_match_growth_and_drop(self):
        path = 'client-gpu/src/renderer.rs'
        self.baseline({path: 1}, {})
        self.source(path, 'let x = "botania:pool";')
        self.check(0)
        self.source(path, 'let x = "botania:pool";\nlet y = "create:shaft";')
        self.check(1, 'mod_namespace_spread_ratchet')
        self.source(path, 'let x = "minecraft:stone";')
        data = self.check(0)
        self.assertEqual(data['improvements'][0]['current'], 0)

    def test_new_file_spread_fails(self):
        self.source('client-app/src/new.rs', 'let block = "thermal:machine";')
        self.check(1, 'mod_namespace_spread_ratchet')

    def test_trap_ratchet_never_grows(self):
        path = 'src/blocks.rs'
        line = 'name.starts_with("minecraft:") && name.ends_with("_bed")\n'
        self.baseline({}, {path: 1})
        self.source(path, line)
        self.check(0)
        self.source(path, line * 2)
        self.check(1, 'vanilla_only_namespace_suffix_trap')

    def test_update_baseline_never_raises(self):
        self.source('src/blocks.rs', 'let a = "create:shaft";')
        result = self.run_audit('--update-baseline')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads((self.root / 'audit/hardcode_guard_baseline.json').read_text())['counts'], {})

    def test_live_repository_passes(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--root', str(ROOT)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_ci_calls_both_gates(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        self.assertIn('python3 tools/hardcode_guard_audit.py', workflow)
        self.assertIn('python3 tests/hardcode_guard_test.py', workflow)


if __name__ == '__main__':
    unittest.main()
