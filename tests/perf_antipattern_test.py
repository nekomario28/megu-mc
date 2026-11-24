#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Synthetic regression tests for tools/perf_antipattern_audit.py.

Each case copies the guard's real inputs (scanned sources + ratchet baseline)
into a temp root, injects one anti-pattern, and proves the gate fails with a
file:line finding — plus the negative controls proving comments, literals and
#[cfg(test)] items cannot trip the same checks.
"""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GUARD = ROOT / 'tools/perf_antipattern_audit.py'


def run(*args):
    return subprocess.run([sys.executable, str(GUARD), *map(str, args)],
                          cwd=ROOT, capture_output=True, text=True, check=False)


class PerfAntipatternTests(unittest.TestCase):
    def test_live_repository_passes(self):
        result = run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_clean_fixture_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            result = run('--root', root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_injected_regressions_fail(self):
        cases = (
            ('client-gpu/src/window.rs',
             'fn render_audit_probe() { std::fs::write("x", b"y"); }',
             'frame_blocking'),
            ('client-gpu/src/live_session.rs',
             'fn render_voxel_probe() { std::thread::sleep(std::time::Duration::from_millis(1)); }',
             'frame_blocking'),
            ('client-gpu/src/renderer.rs',
             'fn draw_audit_probe(device: &Device) { let _ = device.create_buffer(&d()); }',
             'frame_gpu_create'),
            ('client-gpu/src/window.rs',
             'fn encode_audit_probe(device: &Device) { let _ = device.create_texture(&d()); }',
             'frame_gpu_create'),
            ('src/main.rs', 'fn audit_probe(v: &mut Vec<i32>) { v.remove(0); }',
             'vec_remove_zero'),
            ('src/worldgen.rs',
             'fn audit_probe() { std::thread::sleep(std::time::Duration::from_millis(1)); }',
             'server_thread_sleep'),
            ('src/feature.rs',
             'fn audit_probe() { for _ in 0..4 { let _ = std::time::SystemTime::now(); } }',
             'hot_loop_systemtime'),
            ('src/worldgen.rs',
             'fn audit_probe(&self) { for _ in 0..4 { let _ = self.blocks.clone(); } }',
             'hot_loop_map_clone'),
            ('client-gpu/src/live_session.rs',
             'fn audit_probe(w: &W) {\n    for x in 0..16 {\n        for y in 0..16 {\n'
             '            for z in 0..16 {\n                let _ = w.get_block_state(x, y, z);\n'
             '            }\n        }\n    }\n}',
             'uncached_cube_scan'),
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            for name, snippet, rule in cases:
                with self.subTest(rule=rule, file=name):
                    path = root / name
                    original = path.read_text(encoding='utf-8')
                    path.write_text(snippet + '\n' + original, encoding='utf-8')
                    result = run('--root', root)
                    self.assertEqual(result.returncode, 1,
                                     f'{rule} not caught: {result.stdout + result.stderr}')
                    self.assertIn(f'{name}:', result.stdout)
                    self.assertIn(rule, result.stdout)
                    path.write_text(original, encoding='utf-8')

    def test_masked_regions_cannot_trip_checks(self):
        # The same violations inside a comment, a string literal or a
        # cfg(test) item must stay invisible to the gate.
        snippet = (
            '// std::thread::sleep(std::time::Duration::from_millis(1));\n'
            'fn audit_probe() {\n'
            '    let _ = "std::fs::write(\\"x\\", b\\"y\\")";\n'
            '    /* v.remove(0); */\n'
            '}\n'
            '#[cfg(test)]\n'
            'mod audit_tests {\n'
            '    fn t() { std::thread::sleep(std::time::Duration::from_millis(1));\n'
            '             let mut v = Vec::new(); v.remove(0); }\n'
            '}\n'
            '#[cfg(all(test, feature = "extra"))]\n'
            'fn cfg_all_test_probe() { std::thread::sleep(std::time::Duration::from_millis(1)); }\n')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            for name in ('src/worldgen.rs', 'client-gpu/src/window.rs'):
                with self.subTest(file=name):
                    path = root / name
                    original = path.read_text(encoding='utf-8')
                    path.write_text(snippet + original, encoding='utf-8')
                    result = run('--root', root)
                    self.assertEqual(result.returncode, 0,
                                     f'masked code tripped the gate: {result.stdout + result.stderr}')
                    path.write_text(original, encoding='utf-8')

    def test_unwired_optimizer_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            # Both registered optimizers lose every non-test use at once.
            (root / 'src/live_mod_bridge.rs').write_text('fn audit_probe() {}\n', encoding='utf-8')
            result = run('--root', root)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn('unwired_optimizer', result.stdout)
            self.assertIn('AutoRecipeMemoizer', result.stdout)
            self.assertIn('AdaptivePollBackoff', result.stdout)

    def test_decl_head_and_import_do_not_count_as_use(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            (root / 'src/live_mod_bridge.rs').write_text(
                'struct AutoRecipeMemoizer;\n'
                'type AdaptivePollBackoff = u32;\n'
                'use crate::x::{AutoRecipeMemoizer,\n    AdaptivePollBackoff};\n',
                encoding='utf-8')
            result = run('--root', root)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn('unwired_optimizer', result.stdout)

    def test_json_report_and_stale_baseline(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            result = run('--root', root, '--json')
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            report = json.loads(result.stdout)
            self.assertTrue(report['ok'])
            self.assertFalse(report['findings'])
            self.assertEqual(report['files_scanned'], 11)
            self.assertIn('vec_remove_zero', report['counts'])
            # A ceiling above the current count is a stale-baseline tip only —
            # pass normally, fail under --fail-on-stale.
            baseline = json.loads(
                (root / 'audit/perf_antipattern_baseline.json').read_text(encoding='utf-8'))
            baseline['vec_remove_zero']['src/main.rs'] += 1
            (root / 'audit/perf_antipattern_baseline.json').write_text(
                json.dumps(baseline), encoding='utf-8')
            result = run('--root', root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('DOWN', result.stdout)
            result = run('--root', root, '--fail-on-stale')
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_update_baseline_never_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.copy_audit_inputs(root)
            (root / 'audit/perf_antipattern_baseline.json').unlink()
            result = run('--root', root, '--update-baseline')
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            written = json.loads(
                (root / 'audit/perf_antipattern_baseline.json').read_text(encoding='utf-8'))
            self.assertEqual(written['vec_remove_zero']['src/main.rs'], 1)
            # Baselines ratchet downward only: a hand-raised ceiling is pulled
            # back to the real count, never silently kept.
            written['vec_remove_zero']['src/main.rs'] = 99
            (root / 'audit/perf_antipattern_baseline.json').write_text(
                json.dumps(written), encoding='utf-8')
            result = run('--root', root, '--update-baseline')
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            written = json.loads(
                (root / 'audit/perf_antipattern_baseline.json').read_text(encoding='utf-8'))
            self.assertEqual(written['vec_remove_zero']['src/main.rs'], 1)

    def test_ci_calls_both_gates(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        self.assertIn('python3 tools/perf_antipattern_audit.py', workflow)
        self.assertIn('python3 tests/perf_antipattern_test.py', workflow)

    @staticmethod
    def copy_audit_inputs(root):
        sys.path.insert(0, str(ROOT / 'tools'))
        from perf_antipattern_audit import BASELINE, RULES, WIRED
        names = {BASELINE}
        for _, files, _ in RULES:
            names.update(files)
        for _, _, candidates, _ in WIRED:
            for pattern in candidates:
                if '*' in pattern:
                    names.update(p.relative_to(ROOT).as_posix()
                                 for p in ROOT.glob(pattern) if p.is_file())
                else:
                    names.add(pattern)
        for name in names:
            dest = root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, dest)


if __name__ == '__main__':
    unittest.main()
