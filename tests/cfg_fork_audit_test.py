#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Pure-Python hermetic regression tests for the cfg fork audit."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / 'tools/cfg_fork_audit.py'
spec = importlib.util.spec_from_file_location('cfg_fork_audit', SCRIPT)
audit_tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit_tool)


class CfgForkAuditTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'src').mkdir()
        self.baseline({})

    def baseline(self, counts):
        file = self.root / 'audit/cfg_fork_baseline.json'
        file.parent.mkdir(exist_ok=True)
        file.write_text(json.dumps({'counts': counts}))

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

    def test_clean_production_file(self):
        self.source('src/lib.rs', 'pub fn add(a: i32, b: i32) -> i32 { a + b }\n')
        self.check(0)

    def test_legitimate_cfg_test_module_passes(self):
        self.source('src/blocks.rs', '''#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(1, 1);
        if cfg!(test) { panic!() }
    }
}
''')
        data = self.check(0)
        self.assertEqual(data['counts'], {})

    def test_compound_cfg_test_module_passes(self):
        self.source('src/blocks.rs', '''#[cfg(all(test, megu_vanilla_data))]
mod tests {
    fn helper() { let _ = cfg!(test); }
}
''')
        self.check(0)

    def test_cfg_test_mod_semicolon_declaration_passes(self):
        self.source('src/main.rs', '''#[cfg(test)]
mod brigadier_tests;
#[cfg(all(test, megu_vanilla_data))]
#[path = "chest_loot_tests.rs"]
mod tests;
''')
        self.check(0)

    def test_cfg_test_on_enum_variant_fails(self):
        self.source('src/kind.rs', '''pub enum Kind {
    Stone,
    #[cfg(test)]
    FixtureOnly,
}
''')
        findings = self.check(1, 'cfg_test_fork_in_production')['findings']
        self.assertEqual(findings[0]['line'], 3)

    def test_cfg_all_test_on_struct_field_fails(self):
        self.source('src/state.rs', '''pub struct State {
    pub real: i32,
    #[cfg(all(test, megu_vanilla_data))]
    pub fixture: i32,
}
''')
        self.check(1, 'cfg_test_fork_in_production')

    def test_cfg_test_on_match_arm_fails(self):
        self.source('src/match.rs', '''pub fn pick(x: i32) -> i32 {
    match x {
        0 => 1,
        #[cfg(test)]
        _ => 2,
    }
}
''')
        self.check(1, 'cfg_test_fork_in_production')

    def test_cfg_test_on_fn_item_fails(self):
        self.source('src/helper.rs', '''fn production() {}
#[cfg(test)]
fn test_only_helper() {}
''')
        findings = self.check(1, 'cfg_test_fork_in_production')['findings']
        self.assertEqual(findings[0]['line'], 2)

    def test_cfg_test_on_const_and_static_fail(self):
        self.source('src/tables.rs', '''const A: i32 = 1;
#[cfg(test)]
const B: i32 = 2;
#[cfg(test)]
static C: i32 = 3;
''')
        data = self.check(1, 'cfg_test_fork_in_production')
        self.assertEqual(len(data['findings']), 2)

    def test_cfg_not_test_anywhere_fails(self):
        self.source('src/prod.rs', '''pub fn f() {
    #[cfg(not(test))]
    let x = 1;
    #[cfg(not(test))]
    let y = 2;
}
''')
        data = self.check(1, 'cfg_test_negation_in_production')
        self.assertEqual(len(data['findings']), 2)

    def test_cfg_not_test_on_mod_still_fails(self):
        self.source('src/prod.rs', '''#[cfg(not(test))]
mod prod_only;
''')
        self.check(1, 'cfg_test_negation_in_production')

    def test_cfg_test_macro_in_production_fails(self):
        self.source('src/prod.rs', 'let fast = if cfg!(test) { 1 } else { 2 };\n')
        self.check(1, 'cfg_test_macro_in_production')

    def test_cfg_macro_compound_test_fails(self):
        self.source('src/prod.rs', 'let fast = cfg!(all(test, feature = "x"));\n')
        self.check(1, 'cfg_test_macro_in_production')

    def test_cfg_macro_without_test_passes(self):
        self.source('src/prod.rs', 'let is_win = cfg!(windows);\n')
        self.check(0)

    def test_comments_and_strings_cannot_forge(self):
        self.source('src/prod.rs', '''// #[cfg(test)] fn fake() {}
/* cfg!(test) */
let s = "#[cfg(test)]";
let t = "cfg!(test)";
''')
        self.check(0)

    def test_feature_named_test_does_not_forge(self):
        self.source('src/prod.rs', '#[cfg(feature = "test-utils")]\nfn f() {}\n')
        self.check(0)

    def test_nested_block_comments_and_raw_strings(self):
        self.source('src/prod.rs', '''/* outer /* #[cfg(test)] */ still comment */
let r = r#"cfg!(test)"#;
#[cfg(test)]
fn real_fork() {}
''')
        data = self.check(1, 'cfg_test_fork_in_production')
        self.assertEqual(len(data['findings']), 1)
        self.assertEqual(data['findings'][0]['line'], 3)

    def test_ratchet_blocks_growth_allows_existing(self):
        path = 'src/blocks.rs'
        line = '#[cfg(test)]\nfn helper() {}\n'
        self.baseline({path: 1})
        self.source(path, line)
        self.check(0)
        self.source(path, line + '#[cfg(test)]\nfn another() {}\n')
        self.check(1, 'cfg_test_fork_in_production')

    def test_unlisted_file_has_zero_ceiling(self):
        self.source('src/new_file.rs', '#[cfg(test)]\nfn helper() {}\n')
        self.check(1, 'cfg_test_fork_in_production')

    def test_baseline_reduction_prints_down_hint(self):
        path = 'src/blocks.rs'
        self.baseline({path: 2})
        self.source(path, '#[cfg(test)]\nfn helper() {}\n')
        result = subprocess.run([sys.executable, str(SCRIPT), '--root', str(self.root)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f'DOWN {path}: 1 < 2', result.stdout)

    def test_update_baseline_never_raises(self):
        self.baseline({'src/blocks.rs': 1})
        self.source('src/blocks.rs', '#[cfg(test)]\nfn a() {}\n#[cfg(test)]\nfn b() {}\n')
        result = self.run_audit('--update-baseline')
        self.assertEqual(result.returncode, 1)
        data = json.loads((self.root / 'audit/cfg_fork_baseline.json').read_text())
        self.assertEqual(data['counts'], {'src/blocks.rs': 1})

    def test_update_baseline_lowers(self):
        path = 'src/blocks.rs'
        self.baseline({path: 3})
        self.source(path, '#[cfg(test)]\nfn helper() {}\n')
        result = self.run_audit('--update-baseline')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        data = json.loads((self.root / 'audit/cfg_fork_baseline.json').read_text())
        self.assertEqual(data['counts'], {path: 1})

    def test_workspace_crate_sources_scanned(self):
        self.source('client-port/src/lib.rs', '#[cfg(test)]\nfn helper() {}\n')
        (self.root / 'client-port').mkdir(exist_ok=True)
        (self.root / 'client-port/Cargo.toml').write_text('[package]\nname = "p"\n')
        self.check(1, 'cfg_test_fork_in_production')

    def test_excluded_directories_not_scanned(self):
        self.source('client-port/tests/integration.rs', '#[cfg(test)]\nfn helper() {}\n')
        (self.root / 'client-port').mkdir(exist_ok=True)
        (self.root / 'client-port/Cargo.toml').write_text('[package]\nname = "p"\n')
        self.check(0)

    def test_live_repository_passes(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--root', str(ROOT)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
