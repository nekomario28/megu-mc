#!/usr/bin/env python3
"""Exercise the public export without a jar, network, or changes to the source checkout."""
import argparse
import json
import os
import re
import subprocess
import tarfile
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXPORT = ROOT / 'tools/export_public_branch.sh'
TSVS = ('biome_probe', 'vanilla_vectors', 'biome_probe_ties', 'random_next_int')
SUITE = ('megu', 'megu-client', 'megu-mod-check',
         'megu-world-migrate', 'megu-init')


def run(cwd, *cmd, expect=0, env=None):
    proc = subprocess.run(cmd, cwd=cwd, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, env=env)
    if proc.returncode != expect:
        raise AssertionError(f'{cmd} exited {proc.returncode}, expected {expect}:\n{proc.stdout}')
    return proc.stdout


def check_docs(root):
    assert re.search(r'^license\s*=\s*"LGPL-3.0-or-later"$', (root / 'Cargo.toml').read_text(), re.M)
    assert re.search(r'GNU LESSER GENERAL PUBLIC LICENSE\s+Version 3',
                     (root / 'LICENSE').read_text())
    assert re.search(r'GNU LESSER GENERAL PUBLIC LICENSE\s+Version 3',
                     (root / 'COPYING.LESSER').read_text())
    assert re.search(r'GNU GENERAL PUBLIC LICENSE\s+Version 3',
                     (root / 'COPYING.GPL').read_text())
    notice = (root / 'NOTICE').read_text()
    assert 'does not contain Mojang data' not in ' '.join(notice.split())
    for name in TSVS:
        assert f'tests/data/{name}_1211.tsv' in notice, name
    for doc in ('README.md', 'docs/development.md', 'THIRD_PARTY.md', 'LICENSING.md', 'NOTICE'):
        assert 'tools/export_public_branch.sh' in (root / doc).read_text(), doc


def check_public_licenses(root):
    # Inspect every exported package, including workspace members that the
    # provenance gate's explicit manifest list may not yet know about.
    manifests = sorted(root.rglob('Cargo.toml'))
    assert manifests
    for path in manifests:
        package = tomllib.loads(path.read_text()).get('package')
        if package is None:
            continue
        license_id = package.get('license', '')
        if not license_id:
            continue  # Developer-only probe/benchmark manifests can omit this field.
        incompatible = re.search(
            r'(?i)(?<![A-Za-z0-9])(?:polyform|arr|busl|sspl|proprietary)'
            r'(?![A-Za-z0-9])|all[ _-]+rights[ _-]+reserved', license_id,
        )
        assert not incompatible, f'{path}: incompatible license {license_id!r}'


def check_exported_bundle(out, tmp):
    # An exported checkout must be able to package the real guide and the
    # whole binary suite without importing generated Minecraft data.
    binaries = tmp / 'suite'
    binaries.mkdir()
    for name in SUITE:
        path = binaries / name
        path.write_bytes(f'\x7fELF exported-suite fixture {name}\n'.encode())
        path.chmod(0o755)

    artifacts = tmp / 'artifacts'
    args = ['python3', 'tools/release_bundle.py', 'build',
            '--binary', binaries / SUITE[0]]
    for name in SUITE[1:]:
        args.extend(('--extra-binary', binaries / name))
    args.extend(('--out-dir', artifacts, '--version', '0.1.0',
                 '--target', 'x86_64-unknown-linux-gnu', '--repo-root', out))
    run(out, *args)

    prefix = 'megu-0.1.0-x86_64-unknown-linux-gnu'
    bundle = artifacts / f'{prefix}.tar.gz'
    manifest = artifacts / 'release-manifest.json'
    run(out, 'python3', 'tools/release_bundle.py', 'verify',
        '--bundle', bundle, '--manifest', manifest, '--sums', artifacts / 'SHA256SUMS')
    records = {record['path']: record for record in json.loads(manifest.read_text())['files']}
    assert {f'{prefix}/{name}' for name in SUITE} <= set(records)
    assert records[f'{prefix}/docs/mod-author-guide.md']['mode'] == '0644'
    with tarfile.open(bundle, 'r:gz') as archive:
        names = {member.name for member in archive.getmembers()}
        assert names == set(records)
        assert not [name for name in names if '_1211' in name or name.endswith(('.jar', '.class'))]
        guide = archive.extractfile(f'{prefix}/docs/mod-author-guide.md').read()
        assert guide == (out / 'docs/mod-author-guide.md').read_bytes()


def test_export(from_worktree):
    # The exporter requires its destination to be outside the source tree,
    # even when the caller sets TMPDIR to the checkout's target/tmp.
    with tempfile.TemporaryDirectory(prefix='public-export-test-', dir='/tmp') as tmp:
        out = Path(tmp) / 'export'
        extra = ['--from-worktree'] if from_worktree else []
        export_env = os.environ.copy()
        for name in ('GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL',
                     'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL'):
            export_env.pop(name, None)
        run(ROOT, EXPORT, '--out', out, '--verify', *extra, env=export_env)
        assert run(out, 'git', 'rev-list', '--count', 'HEAD').strip() == '1'
        assert not run(out, 'git', 'show', '-s', '--format=%P').strip()
        release_identity = 'megu release <release@megu.invalid>'
        identities = run(out, 'git', 'show', '-s', '--format=%an <%ae>%n%cn <%ce>').splitlines()
        assert identities == [release_identity, release_identity], identities
        assert run(out, 'git', 'branch', '--show-current').strip() == 'public-export'
        assert not run(out, 'git', 'status', '--porcelain')
        strict = ('python3', 'tools/provenance_check.py', '--strict-public')
        run(out, *strict)
        for path in ('datapacks/vanilla/', 'config_packets.bin', 'assets/block_states_1211.json',
                     *(f'tests/data/{name}_1211.tsv' for name in TSVS)):
            assert not (out / path).exists(), path
            run(out, 'git', 'check-ignore', '-q', '--no-index', path)
        # Every clean first-party crate, the tools a public user needs to
        # regenerate data and package a release, the container entrypoint, the
        # operator docs, and the manual Pages deploy must survive the export.
        for path in ('core/Cargo.toml', 'proto/Cargo.toml', 'state-store/Cargo.toml',
                     'server-files/Cargo.toml', 'mod-adapter-core/Cargo.toml',
                     'create-adapter/Cargo.toml', 'mod-offload/Cargo.toml',
                     'client-port/Cargo.toml', 'client-kernel/Cargo.toml',
                     'client-app/Cargo.toml', 'tools/vanilla_data/version_pack.py',
                     'tools/vanilla_data/gen_vanilla_data.sh', 'tools/mod_launcher.py',
                     'tools/parity_cache.py', 'tools/release_bundle.py',
                     'tests/companion_suite_e2e.py', 'tests/release_bundle_test.py',
                     'server-files/src/bin/megu_init.rs',
                     'docs/mod-author-guide.md', 'docker/Dockerfile',
                     'docker/entrypoint.sh', 'docs/operator-guide.md',
                     'docs/architecture-crates.md', 'docs/release-checklist.md',
                     '.github/workflows/pages.yml',
                     'website/index.html', 'website/site-config.js',
                     'website/app.js', 'website/style.css',
                     'website/check_site.py'):
            assert (out / path).is_file(), path
        # No generated (*_1211*) or local-state (world_overlay*.bin*) artifact may
        # be left anywhere in the exported tree.
        generated = [p for p in out.rglob('*_1211*') if '.git' not in p.parts]
        assert not generated, generated[:5]
        assert not list(out.rglob('world_overlay*.bin*')), 'world_overlay artifact exported'
        check_docs(out)
        check_public_licenses(out)
        check_exported_bundle(out, Path(tmp))

        # Both forbidden proprietary license identifiers must fail the real
        # strict-public gate even if planted after a clean public export.
        manifest_path = out / 'client-app/Cargo.toml'
        original = manifest_path.read_bytes()
        for incompatible in ('ARR', 'PolyForm-Shield-1.0.0'):
            poisoned = re.sub(r'(?m)^license\s*=.*$',
                              f'license = "{incompatible}"', original.decode())
            assert poisoned != original.decode()
            manifest_path.write_text(poisoned)
            assert 'incompatible license' in run(out, *strict, expect=1), incompatible
        manifest_path.write_bytes(original)
        run(out, *strict)

        assert 'not empty' in run(ROOT, EXPORT, '--out', out, expect=1)
        path = 'config_packets.bin'
        (out / path).write_bytes(b'planted')
        run(out, 'git', 'add', '-f', '--', path)
        assert f'{path} is still tracked' in run(out, *strict, expect=1)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--from-worktree', action='store_true', help='test tracked pre-commit edits')
    test_export(parser.parse_args().from_worktree)
    print('public export: verified root commit, exclusions, docs, and refusal cases')
