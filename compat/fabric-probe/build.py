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

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--java-home', default=os.environ.get('JAVA_HOME'))
    args = parser.parse_args()
    javac = str(pathlib.Path(args.java_home) / 'bin/javac') if args.java_home else 'javac'
    cache, build = ROOT / '.cache', ROOT / '.build'
    cache.mkdir(exist_ok=True); build.mkdir(exist_ok=True)
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
    sources = sorted((ROOT / 'java').rglob('*.java')) + sorted((ROOT / 'fixtures').rglob('*.java'))
    subprocess.run([javac, '-J-Xmx128m', '--release', '21', '-proc:none', '-cp', os.pathsep.join(map(str, libraries)), '-d', str(classes), *map(str, sources)], check=True)
    app = {str(p.relative_to(classes)): p.read_bytes() for p in (classes / 'net/mcspine/probe').rglob('*.class')}
    app['META-INF/services/net.fabricmc.loader.impl.game.GameProvider'] = b'net.mcspine.probe.SpineGameProvider\n'
    jar(build / 'game.jar', app)
    def fixture(name, cls=None, mixin=None, depends=None):
        metadata = {'schemaVersion': 1, 'id': name, 'version': '1.0.0', 'environment': 'server', 'depends': depends or {'fabricloader': '>=0.19.5', 'java': '>=21', 'mcspine_probe': '*'}}
        entries = {}
        if cls:
            file = 'net/mcspine/fixture/' + ('mixin/' if mixin else '') + cls + '.class'
            entries[file] = (classes / file).read_bytes()
        if name == 'place_block':
            metadata['entrypoints'] = {'main': ['net.mcspine.fixture.PlaceBlock']}
        if mixin:
            metadata['mixins'] = [name + '.mixins.json']
            entries[name + '.mixins.json'] = json.dumps({'required': True, 'minVersion': '0.8', 'compatibilityLevel': 'JAVA_21', 'package': 'net.mcspine.fixture.mixin', 'mixins': [mixin], 'injectors': {'defaultRequire': 1}}).encode()
        entries['fabric.mod.json'] = json.dumps(metadata, sort_keys=True).encode()
        jar(build / (name + '.jar'), entries)
    fixture('place_block', 'PlaceBlock')
    fixture('block_guard', 'BlockGuard', 'BlockGuard')
    fixture('invalid_guard', 'InvalidGuard', 'InvalidGuard')
    fixture('requires_minecraft', depends={'minecraft': '1.21.1'})
    (build / 'classpath.txt').write_text(os.pathsep.join(map(str, [build / 'game.jar', *libraries])))
    (build / 'build-receipt.json').write_text(json.dumps({'java_home': args.java_home, 'sources': {str(p.relative_to(ROOT)): sha(p) for p in sources}, 'jars': {p.name: sha(p) for p in sorted(build.glob('*.jar'))}}, indent=2) + '\n')
    print(build / 'classpath.txt')

if __name__ == '__main__':
    main()
