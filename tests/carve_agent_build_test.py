#!/usr/bin/env python3
"""Check the agent builder's publication guards without a JDK or game runtime."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]

# Deliberately not a Java compiler: produce controllable package entries so these
# tests exercise the real builder's jar/marker checks, not javac correctness.
JAVAC = '''#!/usr/bin/env python3
import os, sys
from pathlib import Path
args = sys.argv[1:]
out = Path(args[args.index('-d') + 1])
out.mkdir(parents=True, exist_ok=True)
for source in [Path(p) for p in args if p.endswith('.java')]:
    if source.stem == os.environ.get('FAKE_OMIT_CLASS'):
        continue
    blob = b'SPATCH SPLACE SPASS SDRAW SCURSOR SSPREADALL SRNG'
    if source.stem == 'Agent':
        blob += b' SculkLog ebq edi eiv dyn duz dmu'
        (out / 'SculkTransformer.class').write_bytes(b'transformer')
    marker = os.environ.get('FAKE_OMIT_MARKER')
    if marker:
        blob = blob.replace(marker.encode(), b'')
    (out / (source.stem + '.class')).write_bytes(blob)
'''
JAR = '''#!/usr/bin/env python3
from pathlib import Path
import sys, zipfile
args = sys.argv[1:]
if args[0] == 'xf':
    with zipfile.ZipFile(args[1]) as z:
        for name in z.namelist():
            if name.startswith(args[2] + '/'):
                z.extract(name)
else:
    assert args[0] in ('cf', 'cfm'), args
    with zipfile.ZipFile(args[1], 'w') as z:
        for path in Path('.').rglob('*'):
            if path.is_file():
                z.write(path, str(path))
'''


class AgentBuildGuards(unittest.TestCase):
    def run_builder(self, *, omit_class=None, omit_marker=None, missing_asm=False):
        with tempfile.TemporaryDirectory(prefix='agent-build-contract-') as tmp:
            root = Path(tmp)
            (root / 'tools/carve-agent').mkdir(parents=True)
            shutil.copy2(ROOT / 'tools/build_carve_agent.sh', root / 'tools/build_carve_agent.sh')
            for name in ('Agent', 'CarveLog', 'SculkLog'):
                (root / f'tools/carve-agent/{name}.java').write_text('// compiler input\n')
            (root / 'fake-bin').mkdir()
            for name, content in (('javac', JAVAC), ('jar', JAR)):
                path = root / 'fake-bin' / name
                path.write_text(content)
                path.chmod(0o755)
            asm = root / 'ASM with spaces.jar'
            with zipfile.ZipFile(asm, 'w') as z:
                z.writestr('org/objectweb/asm/ClassWriter.class', b'fake ASM')
            env = dict(os.environ, PATH=str(root / 'fake-bin') + os.pathsep + os.environ['PATH'],
                       ASM_JAR='missing.jar' if missing_asm else asm.name)
            env.pop('FAKE_OMIT_CLASS', None)
            env.pop('FAKE_OMIT_MARKER', None)
            if omit_class:
                env['FAKE_OMIT_CLASS'] = omit_class
            if omit_marker:
                env['FAKE_OMIT_MARKER'] = omit_marker
            out = root / 'output'
            result = subprocess.run(['bash', 'tools/build_carve_agent.sh', str(out)], cwd=root,
                                    env=env, capture_output=True, text=True, timeout=15)
            jars = {}
            for path in out.glob('*.jar'):
                with zipfile.ZipFile(path) as jar:
                    jars[path.name] = jar.namelist()
            return result.returncode, result.stdout + result.stderr, jars

    def test_relative_asm_with_spaces_and_both_loggers_are_packaged(self):
        rc, log, jars = self.run_builder()
        self.assertEqual(rc, 0, log)
        self.assertIn('SculkLog.class', jars['carvelog.jar'])
        self.assertIn('CarveLog.class', jars['carvelog.jar'])
        self.assertIn('org/objectweb/asm/ClassWriter.class', jars['carveagent.jar'])

    def test_missing_logger_cannot_publish_jars(self):
        rc, log, jars = self.run_builder(omit_class='SculkLog')
        self.assertNotEqual(rc, 0)
        self.assertIn('carvelog.jar is missing SculkLog.class', log)
        self.assertEqual(jars, {})

    def test_missing_trace_marker_cannot_publish_jars(self):
        rc, log, jars = self.run_builder(omit_marker='SCURSOR')
        self.assertNotEqual(rc, 0)
        self.assertIn('logger is missing markers: SCURSOR', log)
        self.assertEqual(jars, {})

    def test_missing_agent_wiring_cannot_publish_jars(self):
        rc, log, jars = self.run_builder(omit_marker='dmu')
        self.assertNotEqual(rc, 0)
        self.assertIn('agent does not wire hooks for: dmu', log)
        self.assertEqual(jars, {})

    def test_explicit_missing_asm_does_not_fall_back_to_another_version(self):
        rc, log, jars = self.run_builder(missing_asm=True)
        self.assertNotEqual(rc, 0)
        self.assertIn('ASM_JAR does not exist', log)
        self.assertEqual(jars, {})


if __name__ == '__main__':
    unittest.main()
