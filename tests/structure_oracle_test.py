#!/usr/bin/env python3
"""Check pinned structure assets and native-to-wire transform identity mapping."""
from contextlib import contextmanager, redirect_stdout
import hashlib
import io
import os
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
from structure_transform_oracle import encode
from vanilla_java import JAR_SHA1
from vanilla_structure_assets import export, PREFIXES
import template_processor_oracle as processors
import fossil_oracle as fossils
import voxel_face_oracle as voxels
import scheduled_tick_oracle as scheduled


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

    def test_processor_oracle_passes_native_paths_as_separate_arguments(self):
        @contextmanager
        def runtime(_jar):
            yield ROOT, 'controlled classpath', b''

        def probe(work, cp, source, main, *args, extra_sources):
            self.assertEqual(len(args), 3, 'native argv must contain separate input/output/root paths')
            self.assertTrue(all(isinstance(p, Path) for p in args))
            self.assertEqual(source.name, 'TemplateProcessorOracle.java')
            self.assertEqual({p.name for p in extra_sources}, {'NativeHarness.java', 'StructureTemplateOracle.java'})
            self.assertEqual(args[2], ROOT)
            self.assertEqual(args[0].read_bytes(), processors.INPUT.read_bytes())
            args[1].write_bytes(processors.EXPECTED.read_bytes())

        with patch.object(processors, 'pinned_runtime', runtime), patch.object(processors, 'run_probe', probe), \
                patch.object(processors, 'block_tags', return_value=Path('controlled-tags.tsv')), \
                patch.object(sys, 'argv', ['template_processor_oracle']), patch.dict(os.environ), redirect_stdout(io.StringIO()):
            processors.main()

    def test_processor_oracle_cannot_replace_rows_after_native_failure(self):
        @contextmanager
        def runtime(_jar):
            yield ROOT, 'controlled classpath', b''

        with tempfile.TemporaryDirectory() as temp:
            inputs, expected = Path(temp) / 'inputs.json', Path(temp) / 'expected.tsv'
            inputs.write_bytes(b'preserved inputs')
            expected.write_bytes(b'preserved rows')
            with patch.object(processors, 'INPUT', inputs), patch.object(processors, 'EXPECTED', expected), \
                    patch.object(processors, 'pinned_runtime', runtime), \
                    patch.object(processors, 'run_probe', side_effect=RuntimeError('controlled native failure')), \
                    patch.object(processors, 'block_tags', return_value=Path('controlled-tags.tsv')), \
                    patch.object(sys, 'argv', ['template_processor_oracle', '--write']), patch.dict(os.environ):
                with self.assertRaisesRegex(RuntimeError, 'controlled native failure'):
                    processors.main()
            self.assertEqual(inputs.read_bytes(), b'preserved inputs')
            self.assertEqual(expected.read_bytes(), b'preserved rows')

    def test_fossil_oracle_arguments_and_default_check_are_separate(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''

        def probe(work, cp, source, main, *args, extra_sources):
            self.assertEqual(len(args), 3)
            self.assertTrue(all(isinstance(p, Path) for p in args))
            self.assertEqual(source.name, 'FossilOracle.java')
            self.assertEqual([p.name for p in extra_sources], ['NativeHarness.java'])
            self.assertEqual(args[0].read_bytes(), fossils.CASES.read_bytes())
            self.assertNotEqual(args[1], fossils.EXPECTED)
            args[1].write_bytes(fossils.EXPECTED.read_bytes())
            self.assertNotEqual(args[2], fossils.PROTO_EXPECTED)
            args[2].write_bytes(fossils.PROTO_EXPECTED.read_bytes())

        with patch.object(fossils, 'pinned_runtime', runtime), patch.object(fossils, 'run_probe', probe), \
                patch.object(fossils, 'block_tags', return_value=Path('controlled-tags')), \
                patch.object(fossils, 'fluid_tags', return_value=Path('controlled-fluids')), \
                patch.object(sys, 'argv', ['fossil_oracle']), patch.dict(os.environ), redirect_stdout(io.StringIO()):
            self.assertEqual(fossils.main(), 0)

    def test_fossil_oracle_native_failure_cannot_replace_expected(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''

        with tempfile.TemporaryDirectory() as tmp:
            expected = Path(tmp) / 'expected.tsv'
            expected.write_bytes(b'preserved')
            with patch.object(fossils, 'EXPECTED', expected), patch.object(fossils, 'pinned_runtime', runtime), \
                    patch.object(fossils, 'run_probe', side_effect=RuntimeError('controlled native failure')), \
                    patch.object(fossils, 'block_tags', return_value=Path('controlled-tags')), \
                    patch.object(fossils, 'fluid_tags', return_value=Path('controlled-fluids')), \
                    patch.object(sys, 'argv', ['fossil_oracle', '--write']), patch.dict(os.environ):
                with self.assertRaisesRegex(RuntimeError, 'controlled native failure'):
                    fossils.main()
            self.assertEqual(expected.read_bytes(), b'preserved')

    def test_proto_order_validation_rejects_missing_misgrouped_and_duplicate_ticks(self):
        cases = json.loads(fossils.CASES.read_text())
        placement = fossils.EXPECTED.read_text()
        original = fossils.PROTO_EXPECTED.read_text()
        fossils.validate_proto(original, cases, placement)
        with self.assertRaises(ValueError):
            fossils.validate_proto('', cases, placement)
        rows = original.splitlines()
        index = next(i for i, row in enumerate(rows) if json.loads(row.split('\t')[1]))
        identifier, text = rows[index].split('\t')
        groups = json.loads(text)
        chunk = next(iter(groups))
        for broken in [{ '99999,99999': groups[chunk] },
                       {chunk: groups[chunk] + groups[chunk][:1]}, {}]:
            with self.subTest(broken=broken), self.assertRaises(ValueError):
                edited = rows.copy()
                edited[index] = identifier + '\t' + json.dumps(broken)
                fossils.validate_proto('\n'.join(edited), cases, placement)

    def test_incomplete_native_proto_output_cannot_replace_either_expectation(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''
        def probe(work, cp, source, main, *args, extra_sources):
            args[1].write_bytes(fossils.EXPECTED.read_bytes())
            args[2].write_text('')
        with tempfile.TemporaryDirectory() as tmp:
            expected = Path(tmp) / 'placement.tsv'
            proto = Path(tmp) / 'order.tsv'
            expected.write_bytes(b'preserved placement')
            proto.write_bytes(b'preserved proto order')
            with patch.object(fossils, 'pinned_runtime', runtime), patch.object(fossils, 'run_probe', probe), \
                    patch.object(fossils, 'block_tags', return_value=Path('controlled-tags')), \
                    patch.object(fossils, 'fluid_tags', return_value=Path('controlled-fluids')), \
                    patch.object(sys, 'argv', ['fossil_oracle', '--write', '--expected', str(expected), '--proto-expected', str(proto)]), patch.dict(os.environ):
                with self.assertRaisesRegex(ValueError, 'omitted cases'):
                    fossils.main()
            self.assertEqual(expected.read_bytes(), b'preserved placement')
            self.assertEqual(proto.read_bytes(), b'preserved proto order')

    def test_voxel_adapter_checks_native_argument_boundaries(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''
        def probe(work, cp, source, main, *args, extra_sources):
            self.assertEqual(len(args), 2)
            self.assertTrue(all(isinstance(p, Path) for p in args))
            self.assertEqual(source.name, 'VoxelFaceOracle.java')
            self.assertEqual([p.name for p in extra_sources], ['NativeHarness.java'])
            self.assertEqual(args[0].read_bytes(), (ROOT / 'tests/data/voxel_face_cases_1211.json').read_bytes())
            args[1].write_bytes((ROOT / 'tests/data/voxel_faces_1211.tsv').read_bytes())
        with patch.object(voxels, 'pinned_runtime', runtime), patch.object(voxels, 'run_probe', probe), \
                patch.object(sys, 'argv', ['voxel_face_oracle']), redirect_stdout(io.StringIO()):
            self.assertEqual(voxels.main(), 0)

    def test_voxel_native_failure_or_empty_output_preserves_expectations(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''
        def empty(work, cp, source, main, *args, extra_sources):
            args[1].write_text('')
        with tempfile.TemporaryDirectory() as tmp:
            expected = Path(tmp) / 'expected.tsv'
            expected.write_bytes(b'preserved')
            for failure, error in [(RuntimeError('controlled native failure'), RuntimeError), (empty, ValueError)]:
                with self.subTest(error=error), patch.object(voxels, 'pinned_runtime', runtime), \
                        patch.object(voxels, 'run_probe', side_effect=failure), \
                        patch.object(sys, 'argv', ['voxel_face_oracle', '--write', '--expected', str(expected)]):
                    with self.assertRaises(error):
                        voxels.main()
                self.assertEqual(expected.read_bytes(), b'preserved')

    def test_scheduled_oracle_checks_both_native_stages(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''
        stages = []
        def probe(work, cp, source, main, *args, **kwargs):
            self.assertEqual(cp, 'controlled classpath')
            self.assertEqual(kwargs, {})
            self.assertTrue(all(isinstance(path, Path) for path in args))
            if source.name == 'ScheduledTickOracle.java':
                self.assertEqual(main, 'dev.megu.tools.ScheduledTickOracle')
                self.assertEqual(args, (work / 'scheduled.tsv', work / 'proto.tsv'))
                args[0].write_bytes(scheduled.VECTORS.read_bytes())
                args[1].write_bytes(scheduled.PROTO_VECTORS.read_bytes())
                stages.append('collection')
            else:
                self.assertEqual(source.name, 'ScheduledTickDeliveryOracle.java')
                self.assertEqual(main, 'dev.megu.tools.ScheduledTickDeliveryOracle')
                self.assertEqual(args, (work / 'delivery.tsv',))
                args[0].write_bytes(scheduled.DELIVERY_VECTORS.read_bytes())
                stages.append('delivery')
        with patch.object(scheduled, 'pinned_runtime', runtime), patch.object(scheduled, 'run_probe', probe), \
                patch.object(sys, 'argv', ['scheduled_tick_oracle']), redirect_stdout(io.StringIO()):
            self.assertIsNone(scheduled.main())
        self.assertEqual(stages, ['collection', 'delivery'])

    def test_scheduled_native_failure_or_missing_callbacks_preserves_both_vectors(self):
        @contextmanager
        def runtime(_jar):
            with tempfile.TemporaryDirectory() as tmp:
                yield Path(tmp), 'controlled classpath', b''
        legacy = scheduled.VECTORS.read_bytes()
        def incomplete(work, cp, source, main, *args, **kwargs):
            args[0].write_bytes(legacy)
            args[1].write_bytes(b'')
        with tempfile.TemporaryDirectory() as tmp:
            original = Path(tmp) / 'scheduled.tsv'
            proto = Path(tmp) / 'proto.tsv'
            for failure, error in [(RuntimeError('controlled native failure'), RuntimeError), (incomplete, ValueError)]:
                original.write_bytes(b'preserved original vectors')
                proto.write_bytes(b'preserved proto vectors')
                with self.subTest(error=error), patch.object(scheduled, 'VECTORS', original), \
                        patch.object(scheduled, 'PROTO_VECTORS', proto), patch.object(scheduled, 'pinned_runtime', runtime), \
                        patch.object(scheduled, 'run_probe', side_effect=failure), \
                        patch.object(sys, 'argv', ['scheduled_tick_oracle', '--write']):
                    with self.assertRaises(error):
                        scheduled.main()
                self.assertEqual(original.read_bytes(), b'preserved original vectors')
                self.assertEqual(proto.read_bytes(), b'preserved proto vectors')

    def test_scheduled_proto_validation_rejects_a_stage_mismatch(self):
        rows = scheduled.PROTO_VECTORS.read_text().splitlines()
        fields = rows[0].split('\t')
        values = fields[5].split(';')
        entry = values[0].split(',')
        entry[4] = '100'
        values[0] = ','.join(entry)
        fields[5] = ';'.join(values)
        rows[0] = '\t'.join(fields)
        with self.assertRaisesRegex(ValueError, 'zero delay'):
            scheduled.validate_proto(('\n'.join(rows) + '\n').encode())

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
