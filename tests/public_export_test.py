#!/usr/bin/env python3
"""Exercise the public export without a jar, network, or changes to the source checkout."""
import argparse
import re
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXPORT = ROOT / 'tools/export_public_branch.sh'
TSVS = ('biome_probe', 'vanilla_vectors', 'biome_probe_ties', 'random_next_int')


def run(cwd, *cmd, expect=0):
    proc = subprocess.run(cmd, cwd=cwd, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT)
    if proc.returncode != expect:
        raise AssertionError(f'{cmd} exited {proc.returncode}, expected {expect}:\n{proc.stdout}')
    return proc.stdout


def check_docs(root):
    assert re.search(r'^license\s*=\s*"GPL-3.0-or-later"$', (root / 'Cargo.toml').read_text(), re.M)
    assert re.search(r'GNU GENERAL PUBLIC LICENSE\s+Version 3', (root / 'LICENSE').read_text())
    notice = (root / 'NOTICE').read_text()
    assert 'does not contain Mojang data' not in ' '.join(notice.split())
    for name in TSVS:
        assert f'tests/data/{name}_1211.tsv' in notice, name
    for doc in ('README.md', 'docs/development.md', 'THIRD_PARTY.md', 'LICENSING.md', 'NOTICE'):
        assert 'tools/export_public_branch.sh' in (root / doc).read_text(), doc


def test_export(from_worktree):
    with tempfile.TemporaryDirectory(prefix='public-export-test-') as tmp:
        out = Path(tmp) / 'export'
        extra = ['--from-worktree'] if from_worktree else []
        run(ROOT, EXPORT, '--out', out, '--verify', *extra)
        assert run(out, 'git', 'rev-list', '--count', 'HEAD').strip() == '1'
        assert not run(out, 'git', 'show', '-s', '--format=%P').strip()
        assert run(out, 'git', 'branch', '--show-current').strip() == 'public-export'
        assert not run(out, 'git', 'status', '--porcelain')
        strict = ('python3', 'tools/provenance_check.py', '--strict-public')
        run(out, *strict)
        for path in ('datapacks/vanilla/', 'config_packets.bin', 'assets/block_states_1211.json',
                     *(f'tests/data/{name}_1211.tsv' for name in TSVS)):
            assert not (out / path).exists(), path
            run(out, 'git', 'check-ignore', '-q', '--no-index', path)
        check_docs(out)

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
