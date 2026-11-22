#!/usr/bin/env python3
"""Build pinned diagnostic fixtures; no Minecraft server or Gradle required."""
import argparse, hashlib, json, os, pathlib, subprocess, shutil, urllib.request, zipfile
ROOT = pathlib.Path(__file__).resolve().parent

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def jar(path, entries):
    with zipfile.ZipFile(path, 'w', zipfile.ZIP_DEFLATED) as output:
        for name, data in sorted(entries.items()):
            info = zipfile.ZipInfo(name, (2000, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            output.writestr(info, data)


def build_runtime(build, sources, javac, java, legacy_classes, libraries):
    named = build / 'runtime-named'
    named.mkdir()
    subprocess.run([javac, '-J-Xmx128m', '--release', '21', '-proc:none', '-d', str(named),
                    *map(str, sources)], check=True)
    intermediary = build / 'runtime-intermediary'
    subprocess.run([java, '-Xmx128m', '-cp', os.pathsep.join(map(str, [legacy_classes, *libraries])),
                    'net.megu.probe.RemapFacade', str(ROOT / 'runtime-intermediary.properties'),
                    str(named), str(intermediary)], check=True)
    manifest = b'Manifest-Version: 1.0\r\nMain-Class: net.megu.probe.FabricRuntimeAdapter\r\n\r\n'
    surface = (ROOT / 'shim-surface.json').read_bytes()
    for classes, filename, namespace in ((named, 'fabric-runtime.jar', 'named'),
                                         (intermediary, 'fabric-runtime-intermediary.jar', 'intermediary')):
        entries = {str(p.relative_to(classes)): p.read_bytes() for p in classes.rglob('*.class')}
        entries['META-INF/MANIFEST.MF'] = manifest
        entries['fabric-runtime-namespace'] = (namespace + '\n').encode()
        entries['fabric-runtime-surface.json'] = surface
        jar(build / filename, entries)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--java-home', default=os.environ.get('JAVA_HOME'))
    parser.add_argument('--build-dir', type=pathlib.Path, help='new output directory (default: .build)')
    parser.add_argument('--seedprotect', action='store_true')
    args = parser.parse_args()
    javac = str(pathlib.Path(args.java_home) / 'bin/javac') if args.java_home else 'javac'
    java = str(pathlib.Path(args.java_home) / 'bin/java') if args.java_home else 'java'
    cache = ROOT / '.cache'
    build = args.build_dir.resolve() if args.build_dir else ROOT / '.build'
    cache.mkdir(exist_ok=True)
    if args.build_dir is None and build.exists():
        shutil.rmtree(build)
    build.mkdir()
    libraries = []
    for lib in json.loads((ROOT / 'dependencies.json').read_text())['libraries']:
        path = cache / lib['url'].rsplit('/', 1)[1]
        if not path.exists():
            with urllib.request.urlopen(lib['url'], timeout=30) as response:
                data = response.read(20 * 1024 * 1024 + 1)
            if len(data) > 20 * 1024 * 1024 or hashlib.sha256(data).hexdigest() != lib['sha256']:
                raise RuntimeError('Invalid download: ' + lib['name'])
            path.write_bytes(data)
        if sha(path) != lib['sha256']:
            raise RuntimeError('Dependency hash mismatch: ' + lib['name'])
        libraries.append(path)
    classes = build / 'classes'
    if classes.exists(): shutil.rmtree(classes)
    classes.mkdir()
    runtime = ROOT / 'java/runtime'
    sources = sorted(p for p in (ROOT / 'java').rglob('*.java') if not p.is_relative_to(runtime))
    sources += sorted((ROOT / 'fixtures').rglob('*.java'))
    runtime_sources = sorted(runtime.rglob('*.java'))
    subprocess.run([javac, '-J-Xmx128m', '--release', '21', '-proc:none', '-cp', os.pathsep.join(map(str, libraries)), '-d', str(classes), *map(str, sources)], check=True)
    build_runtime(build, runtime_sources, javac, java, classes, libraries)
    app = {str(p.relative_to(classes)): p.read_bytes() for p in (classes / 'net/megu/probe').rglob('*.class')}
    app['META-INF/services/net.fabricmc.loader.impl.game.GameProvider'] = b'net.megu.probe.SpineGameProvider\n'
    jar(build / 'game.jar', app)
    def fixture(name, cls=None, mixin=None, depends=None):
        metadata = {'schemaVersion': 1, 'id': name, 'version': '1.0.0', 'environment': 'server', 'depends': depends or {'fabricloader': '>=0.19.5', 'java': '>=21', 'megu_probe': '*'}}
        entries = {}
        if cls:
            file = 'net/megu/fixture/' + ('mixin/' if mixin else '') + cls + '.class'
            entries[file] = (classes / file).read_bytes()
        if name == 'place_block':
            metadata['entrypoints'] = {'main': ['net.megu.fixture.PlaceBlock']}
        if mixin:
            metadata['mixins'] = [name + '.mixins.json']
            entries[name + '.mixins.json'] = json.dumps({'required': True, 'minVersion': '0.8', 'compatibilityLevel': 'JAVA_21', 'package': 'net.megu.fixture.mixin', 'mixins': [mixin], 'injectors': {'defaultRequire': 1}}).encode()
        entries['fabric.mod.json'] = json.dumps(metadata, sort_keys=True).encode()
        jar(build / (name + '.jar'), entries)
    fixture('place_block', 'PlaceBlock')
    fixture('block_guard', 'BlockGuard', 'BlockGuard')
    fixture('invalid_guard', 'InvalidGuard', 'InvalidGuard')
    fixture('requires_minecraft', depends={'minecraft': '1.21.1'})
    (build / 'classpath.txt').write_text(os.pathsep.join(map(str, [build / 'game.jar', *libraries])))
    if args.seedprotect:
        from seedprotect_build import prepare
        entries, broken_entries, slf4j = prepare(ROOT, build, classes, libraries, java)
        jar(build / 'game-seedprotect.jar', entries)
        jar(build / 'game-seedprotect-missing-invoke.jar', broken_entries)
        (build / 'classpath-seedprotect-missing-invoke.txt').write_text(os.pathsep.join(map(str, [build / 'game-seedprotect-missing-invoke.jar', *libraries, slf4j])))
        (build / 'classpath-seedprotect.txt').write_text(os.pathsep.join(map(str, [build / 'game-seedprotect.jar', *libraries, slf4j])))
    runtime_inputs = [ROOT / 'runtime-intermediary.properties', ROOT / 'shim-surface.json']
    (build / 'build-receipt.json').write_text(json.dumps({'java_home': args.java_home, 'sources': {str(p.relative_to(ROOT)): sha(p) for p in [*sources, *runtime_sources]}, 'runtime_inputs': {p.name: sha(p) for p in runtime_inputs}, 'jars': {p.name: sha(p) for p in sorted(build.glob('*.jar'))}}, indent=2) + '\n')
    print(build / 'classpath.txt')

if __name__ == '__main__':
    main()
