#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Pure-Python hermetic regression tests for the global lock-order audit."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / 'tools/lock_order_audit.py'

PRELUDE = '''use std::sync::Mutex;
static AX: Mutex<i32> = Mutex::new(0);
static AY: Mutex<i32> = Mutex::new(0);
static AZ: Mutex<i32> = Mutex::new(0);
fn ax() -> &'static Mutex<i32> { &AX }
fn ay() -> &'static Mutex<i32> { &AY }
fn az() -> &'static Mutex<i32> { &AZ }
'''


class LockOrderAuditTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'src').mkdir()
        self.baseline({})

    def baseline(self, counts):
        file = self.root / 'audit/lock_order_baseline.json'
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

    def test_clean_single_lock_file_passes(self):
        self.source('src/lib.rs', PRELUDE + '''fn only_ax() {
    let _a = ax().lock().unwrap();
    let _b = ax().lock().unwrap();
}
''')
        self.check(0)

    def test_same_order_acquisition_passes(self):
        self.source('src/lib.rs', PRELUDE + '''fn one() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
fn two() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
''')
        self.check(0)

    def test_abba_inversion_fails(self):
        self.source('src/lib.rs', PRELUDE + '''fn fwd() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        data = self.check(1, 'lock_order_inversion')
        finding = data['findings'][0]
        self.assertEqual(finding['file'], 'src/lib.rs')
        self.assertEqual(sorted(finding['locks']), ['AX', 'AY'])

    def test_abba_across_files_fails(self):
        self.source('src/a.rs', PRELUDE + '''fn fwd() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
''')
        self.source('src/b.rs', '''use crate::{ax, ay};
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        # accessors live in a.rs; b.rs has no statics of its own but the
        # provider table is global so the inversion must still be seen
        data = self.check(1, 'lock_order_inversion')
        files = {f['file'] for f in data['findings']}
        self.assertEqual(files, {'src/a.rs', 'src/b.rs'})

    def test_three_or_more_simultaneous_locks_fails(self):
        self.source('src/lib.rs', PRELUDE + '''fn triple() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
    let _c = az().lock().unwrap();
}
''')
        data = self.check(1, 'multi_global_lock')
        self.assertEqual(data['findings'][0]['locks'], ['AX', 'AY', 'AZ'])

    def test_two_simultaneous_locks_pass(self):
        self.source('src/lib.rs', PRELUDE + '''fn pair() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
''')
        self.check(0)

    def test_drop_before_second_lock_is_not_an_edge(self):
        self.source('src/lib.rs', PRELUDE + '''fn seq() {
    let _a = ax().lock().unwrap();
    drop(_a);
    let _b = ay().lock().unwrap();
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        self.check(0)

    def test_block_scope_end_before_second_lock_is_not_an_edge(self):
        self.source('src/lib.rs', PRELUDE + '''fn scoped() {
    {
        let _a = ax().lock().unwrap();
    }
    let _b = ay().lock().unwrap();
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        self.check(0)

    def test_statement_temporary_locks_do_not_overlap(self):
        self.source('src/lib.rs', PRELUDE + '''fn temps() {
    *ax().lock().unwrap() = 1;
    *ay().lock().unwrap() = 2;
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        self.check(0)

    def test_struct_literal_temps_overlap(self):
        self.source('src/lib.rs', PRELUDE + '''struct Both { a: i32, b: i32 }
fn fwd() -> Both {
    Both {
        a: *ax().lock().unwrap(),
        b: *ay().lock().unwrap(),
    }
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        self.check(1, 'lock_order_inversion')

    def test_cfg_test_module_is_ignored(self):
        self.source('src/lib.rs', PRELUDE + '''#[cfg(test)]
mod tests {
    fn bad() {
        let _a = super::ax().lock().unwrap();
        let _b = super::ay().lock().unwrap();
        let _c = super::az().lock().unwrap();
    }
}
''')
        self.check(0)

    def test_compound_cfg_module_still_scanned(self):
        self.source('src/lib.rs', PRELUDE + '''#[cfg(all(test, megu_vanilla_data))]
mod fixture_tests {
    fn bad() {
        let _a = super::ax().lock().unwrap();
        let _b = super::ay().lock().unwrap();
        let _c = super::az().lock().unwrap();
    }
}
''')
        self.check(1, 'multi_global_lock')

    def test_static_direct_lock_chain_detected(self):
        self.source('src/lib.rs', PRELUDE + '''fn fwd() {
    let _a = AX.lock().unwrap();
    let _b = AY.lock().unwrap();
}
fn rev() {
    let _b = AY.lock().unwrap();
    let _a = AX.lock().unwrap();
}
''')
        self.check(1, 'lock_order_inversion')

    def test_type_alias_lock_detected(self):
        self.source('src/lib.rs', '''use std::sync::Mutex;
type Registry = Mutex<i32>;
static REG: Registry = Mutex::new(0);
static OTHER: Mutex<i32> = Mutex::new(0);
fn reg() -> &'static Registry { &REG }
fn other() -> &'static Mutex<i32> { &OTHER }
fn fwd() {
    let _a = reg().lock().unwrap();
    let _b = other().lock().unwrap();
}
fn rev() {
    let _b = other().lock().unwrap();
    let _a = reg().lock().unwrap();
}
''')
        self.check(1, 'lock_order_inversion')

    def test_guard_helper_wrapper_detected(self):
        self.source('src/lib.rs', PRELUDE + '''fn lk(m: &Mutex<i32>) -> std::sync::MutexGuard<'_, i32> {
    m.lock().unwrap()
}
fn fwd() {
    let _a = lk(ax());
    let _b = lk(ay());
}
fn rev() {
    let _b = lk(ay());
    let _a = lk(ax());
}
''')
        self.check(1, 'lock_order_inversion')

    def test_proxy_guard_fn_detected(self):
        self.source('src/lib.rs', PRELUDE + '''fn ax_guard() -> std::sync::MutexGuard<'static, i32> {
    ax().lock().unwrap()
}
fn fwd() {
    let _a = ax_guard();
    let _b = ay().lock().unwrap();
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax_guard();
}
''')
        self.check(1, 'lock_order_inversion')

    def test_ratchet_blocks_growth_allows_existing(self):
        body = PRELUDE + '''fn fwd() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
'''
        path = 'src/lib.rs'
        self.baseline({path: 10})
        # seed the inversion into the baseline via update, then confirm growth fails
        self.source(path, body + '''fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        result = self.run_audit('--update-baseline')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        seeded = json.loads((self.root / 'audit/lock_order_baseline.json').read_text())
        self.assertGreaterEqual(seeded['counts'].get(path, 0), 1)
        # adding another inversion must fail against the seeded ceiling
        self.source(path, body + '''fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
fn rev2() {
    let _c = az().lock().unwrap();
    let _a = ax().lock().unwrap();
}
fn fwd2() {
    let _a = ax().lock().unwrap();
    let _c = az().lock().unwrap();
}
''')
        self.check(1, 'lock_order_inversion')

    def test_unlisted_file_has_zero_ceiling(self):
        self.source('src/lib.rs', PRELUDE)
        self.source('src/new.rs', '''fn fwd() {
    let _a = crate::ax().lock().unwrap();
    let _b = crate::ay().lock().unwrap();
}
fn rev() {
    let _b = crate::ay().lock().unwrap();
    let _a = crate::ax().lock().unwrap();
}
''')
        self.check(1, 'lock_order_inversion')

    def test_baseline_reduction_prints_down_hint(self):
        path = 'src/lib.rs'
        self.baseline({path: 2})
        self.source(path, PRELUDE + '''fn fwd() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
''')
        result = subprocess.run([sys.executable, str(SCRIPT), '--root', str(self.root)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f'DOWN {path}: 1 < 2', result.stdout)

    def test_update_baseline_never_raises(self):
        path = 'src/lib.rs'
        self.baseline({path: 1})
        self.source(path, PRELUDE + '''fn fwd() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
}
fn rev() {
    let _b = ay().lock().unwrap();
    let _a = ax().lock().unwrap();
}
fn triple() {
    let _a = ax().lock().unwrap();
    let _b = ay().lock().unwrap();
    let _c = az().lock().unwrap();
}
''')
        result = self.run_audit('--update-baseline')
        self.assertEqual(result.returncode, 1)
        data = json.loads((self.root / 'audit/lock_order_baseline.json').read_text())
        self.assertEqual(data['counts'], {path: 1})

    def test_live_repository_passes(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--root', str(ROOT)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('files_scanned', json.loads(
            subprocess.run([sys.executable, str(SCRIPT), '--root', str(ROOT), '--json'],
                           capture_output=True, text=True).stdout))


if __name__ == '__main__':
    unittest.main()
