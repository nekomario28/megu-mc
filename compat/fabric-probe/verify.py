#!/usr/bin/env python3
"""Independent processes verify actual native storage, real Mixin and refusal arms."""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, tempfile
ROOT = pathlib.Path(__file__).resolve().parent

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', required=True, type=pathlib.Path)
    p.add_argument('--output', required=True, type=pathlib.Path)
    p.add_argument('--seedprotect', action='store_true')
    args = p.parse_args()
    binary = args.binary.resolve()
    build = ROOT / '.build'
    jars = sorted(build.glob('*.jar')) + sorted((ROOT / '.cache').glob('*.jar'))
    before = {str(path.relative_to(ROOT)): sha(path) for path in jars}
    records = []
    def invoke(cwd, parameters, extra_env=None):
        result = subprocess.run([str(binary), *parameters], cwd=cwd, capture_output=True, text=True, timeout=45, env={**os.environ, **(extra_env or {})})
        return {'args': parameters, 'returncode': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    sources = {str(path.relative_to(ROOT)): sha(path) for directory in ['java', 'fixtures'] for path in sorted((ROOT / directory).rglob('*.java'))}
    build_receipt = json.loads((build / 'build-receipt.json').read_text())
    assert sources == build_receipt['sources'], 'Sources changed since fixture compilation'
    assert {path.name: sha(path) for path in build.glob('*.jar')} == build_receipt['jars'], 'Built JAR mismatch'
    resource_scope = {}
    for line in pathlib.Path('/proc/self/cgroup').read_text().splitlines():
        if line.startswith('0::'):
            group = pathlib.Path('/sys/fs/cgroup') / line[3:].lstrip('/')
            for setting in ['memory.max', 'memory.swap.max', 'cpu.max']:
                resource_scope[setting] = (group / setting).read_text().strip()
    with tempfile.TemporaryDirectory(prefix='mcspine-fabric-') as temporary:
        root = pathlib.Path(temporary)
        for name, mods, expected, success in [
            ('api', ['place_block'], 2, True),
            ('mixin_cancel', ['place_block', 'block_guard'], 1, True),
            ('invalid_mixin', ['place_block', 'invalid_guard'], 2, False),
            ('missing_minecraft', ['place_block', 'requires_minecraft'], 2, False),
            ('wrong_expectation', ['place_block'], 1, False),
        ]:
            cwd = root / name; (cwd / 'mods').mkdir(parents=True)
            for mod in mods: shutil.copyfile(build / (mod + '.jar'), cwd / 'mods' / (mod + '.jar'))
            run = invoke(cwd, ['--fabric-probe', str(build / 'classpath.txt'), str(expected)])
            world = cwd / 'world_overlay.bin'
            assert (run['returncode'] == 0) == success, json.dumps({'case': name, 'run': run})
            assert world.exists() == success, name + ': persistence boundary'
            for mod in mods:
                assert sha(cwd / 'mods' / (mod + '.jar')) == sha(build / (mod + '.jar'))
            reasons = {'invalid_mixin': ['InvalidInjectionException', 'missingMethod'], 'missing_minecraft': ['minecraft, which is missing'], 'wrong_expectation': ['World/Mixin mismatch: state=2, changed=true, expected=1']}
            for reason in reasons.get(name, []):
                assert reason in run['stdout'] + run['stderr'], name + ': wrong failure cause'
            record = {'case': name, 'run': run}
            if success:
                receipt = json.loads(run['stdout'].splitlines()[-1])
                assert receipt == {'stored_state': expected, 'changed': expected == 2}
                reload = invoke(cwd, ['--fabric-probe-reload', str(expected)])
                assert reload['returncode'] == 0, reload
                assert json.loads(reload['stdout'].splitlines()[-1]) == {'reloaded_state': expected}
                record['reload'] = reload
                mismatch = invoke(cwd, ['--fabric-probe-reload', str(3 - expected)])
                assert mismatch['returncode'] != 0 and 'Reload mismatch' in mismatch['stderr']
                record['wrong_reload_refusal'] = mismatch
                saved = sha(world)
                refusal = invoke(cwd, ['--fabric-probe', str(build / 'classpath.txt'), str(expected)])
                assert refusal['returncode'] != 0 and sha(world) == saved
                assert 'Refusing to overwrite an existing world' in refusal['stderr']
                record['existing_world_refusal'] = refusal
                record['world_sha256'] = saved
            records.append(record)
        for name, symlink in [('temporary_file', False), ('temporary_symlink', True), ('dangling_temporary_symlink', True), ('dangling_world_symlink', True)]:
            cwd = root / name; cwd.mkdir()
            target = root / (name + '.target')
            artifact = cwd / ('world_overlay.bin' if name == 'dangling_world_symlink' else 'world_overlay.bin.tmp')
            if symlink:
                artifact.symlink_to(target)
                if 'dangling' not in name: target.write_bytes(b'preserved temporary world')
            else: artifact.write_bytes(b'preserved temporary world')
            prior = target.read_bytes() if target.exists() else None
            run = invoke(cwd, ['--fabric-probe', str(build / 'classpath.txt'), '2'])
            cause = 'Refusing to overwrite an existing world' if name == 'dangling_world_symlink' else 'Refusing to overwrite an existing temporary world'
            assert run['returncode'] != 0 and cause in run['stderr']
            if symlink:
                assert artifact.is_symlink() and artifact.readlink() == target
                assert (target.read_bytes() if target.exists() else None) == prior
            else: assert artifact.read_bytes() == b'preserved temporary world'
            records.append({'case': name, 'run': run})
        if args.seedprotect:
            pin = json.loads((ROOT / 'seedprotect.json').read_text())['mod']
            mod = ROOT / '.cache' / pin['url'].rsplit('/',1)[1]
            assert sha(mod) == pin['sha256']
            for name, with_mod, expected, success, broken, unknown in [
                ('trample_baseline',False,10,True,False,False),
                ('seedprotect_redirect',True,4286,True,False,False),
                ('seedprotect_wrong_expectation',True,10,False,False,False),
                ('missing_invoke_baseline',False,10,True,True,False),
                ('required_redirect_failure',True,10,False,True,False),
                ('unsupported_mod_hash',False,10,False,False,True),
            ]:
                cwd=root/name; cwd.mkdir()
                if with_mod or unknown: (cwd/'mods').mkdir()
                if with_mod: shutil.copyfile(mod,cwd/'mods'/mod.name)
                if unknown: shutil.copyfile(build/'place_block.jar',cwd/'mods/place_block.jar')
                cp=build/('classpath-seedprotect-missing-invoke.txt' if broken else 'classpath-seedprotect.txt')
                run=invoke(cwd,['--fabric-trample',str(cp),str(expected)])
                assert (run['returncode']==0)==success, json.dumps({'case':name,'run':run})
                world=cwd/'world_overlay.bin'
                assert world.exists()==success,name
                if with_mod: assert sha(cwd/'mods'/mod.name)==pin['sha256']
                record={'case':name,'run':run}
                if success:
                    assert json.loads(run['stdout'].splitlines()[-1])=={'stored_state':expected,'changed':expected==10}
                    reload=invoke(cwd,['--fabric-trample-reload',str(expected)])
                    assert reload['returncode']==0 and json.loads(reload['stdout'].splitlines()[-1])=={'reloaded_state':expected},reload
                    record['reload']=reload
                else:
                    reason = 'Unsupported mod hash' if unknown else 'Critical injection failure' if broken else 'World/Mixin mismatch: state=4286, changed=false'
                    assert reason in run['stdout']+run['stderr'],run
                records.append(record)
            for name, setting in [
                ('alternate_addmods', '-Dfabric.addMods='+str(build/'requires_minecraft.jar')),
                ('alternate_modsfolder', '-Dfabric.modsFolder='+str(build)),
                ('development_discovery', '-Dfabric.development=true'),
            ]:
                cwd=root/name; cwd.mkdir()
                run=invoke(cwd,['--fabric-trample',str(build/'classpath-seedprotect.txt'),'10'], {'JAVA_TOOL_OPTIONS':setting})
                assert run['returncode']!=0 and 'Unsupported Fabric override in scoped contract' in run['stderr'],run
                assert not (cwd/'world_overlay.bin').exists()
                records.append({'case':name,'run':run})
    after = {str(path.relative_to(ROOT)): sha(path) for path in jars}
    assert before == after, 'Input JAR mutated'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({'status': 'PASS', 'resource_scope': resource_scope, 'base_revision': revision, 'java_sources': sources, 'rust_source_sha256': sha(ROOT.parent.parent / 'src/fabric_probe.rs'), 'binary_sha256': sha(binary), 'java_home': os.environ.get('JAVA_HOME'), 'jars_before': before, 'jars_after': after, 'cases': records, 'boundary': 'Pinned Seed Protect Redirect contract tested' if args.seedprotect else 'Diagnostic fixtures only; Minecraft API and NeoForge NOT RUN'}, indent=2) + '\n')
    print('PASS: API, real Mixin cancellation, fresh-process persistence, required failure and refusal arms')

if __name__ == '__main__':
    main()
