#!/usr/bin/env python3
"""Export exact compressed templates and structure biome tags from pinned 1.21.1."""
import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tempfile
import zipfile
from vanilla_java import DEFAULT_JAR, JAR_SHA1

ROOT = Path(__file__).resolve().parents[1]
PREFIXES = {'data/minecraft/structure/': ('.nbt', 1180),
            'data/minecraft/tags/worldgen/biome/': ('.json', 70),
            'data/minecraft/tags/worldgen/structure/': ('.json', 13)}


def export(jar, output, write):
    bundler = jar.read_bytes()
    if hashlib.sha1(bundler).hexdigest() != JAR_SHA1:
        raise ValueError('not the pinned 1.21.1 server jar')
    with zipfile.ZipFile(io.BytesIO(bundler)) as archive:
        inner = archive.read('META-INF/versions/1.21.1/server-1.21.1.jar')
    files = {}
    with zipfile.ZipFile(io.BytesIO(inner)) as archive:
        for prefix, (suffix, count) in PREFIXES.items():
            names = sorted(n for n in archive.namelist() if n.startswith(prefix) and n.endswith(suffix))
            if len(names) != count:
                raise ValueError(f'incomplete pinned {prefix}: {len(names)} != {count}')
            for name in names:
                path = PurePosixPath(name)
                if path.is_absolute() or '..' in path.parts or name in files:
                    raise ValueError('invalid or duplicate asset path: ' + name)
                files[name] = archive.read(name)
    provenance = {'minecraft': '1.21.1', 'server_sha1': JAR_SHA1,
                  'inner_sha256': hashlib.sha256(inner).hexdigest(),
                  'members_sha256': {n: hashlib.sha256(b).hexdigest() for n, b in sorted(files.items())}}
    files['structure-provenance.json'] = (json.dumps(provenance, indent=2) + '\n').encode()
    for name, content in files.items():
        target = output / name
        if write:
            target.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(dir=target.parent, delete=False) as stream:
                staged = Path(stream.name)
                try:
                    stream.write(content)
                except BaseException:
                    staged.unlink(missing_ok=True)
                    raise
            try:
                staged.replace(target)
            finally:
                staged.unlink(missing_ok=True)
        elif not target.is_file() or target.read_bytes() != content:
            raise ValueError('missing or changed pinned asset: ' + str(target))
    for prefix, (suffix, _) in PREFIXES.items():
        installed = {p.relative_to(output).as_posix() for p in (output / prefix).rglob('*' + suffix)}
        expected = {name for name in files if name.startswith(prefix)}
        if installed != expected:
            raise ValueError('unexpected assets under ' + prefix)
    print(f'pinned structure assets: {len(files)-1} exact members; ' + ('written' if write else 'match'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--output', type=Path, default=ROOT / 'datapacks/vanilla')
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    export(args.jar, args.output, args.write)


if __name__ == '__main__':
    main()
