#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Guard production Rust against #[cfg(test)] feature forks.

Test-only code belongs in #[cfg(test)] modules. A #[cfg(test)] (or
#[cfg(all(test, ...))]) attribute on a struct field, enum variant, match arm,
const/static or fn item — or a #[cfg(not(test))] / cfg!(test) anywhere in
production code — forks behaviour between test and production builds and must
never slip back in.

Usage: python3 tools/cfg_fork_audit.py [--root DIR] [--json] [--update-baseline]
Exit: 0 pass, 1 regression, 2 invalid input or unreadable baseline/source.
The baseline is a per-file ceiling; updates may lower ceilings but never raise them.
"""

import argparse
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hardcode_guard_audit as hg

ROOT = Path(__file__).resolve().parents[1]
BASELINE = Path('audit/cfg_fork_baseline.json')
EXCLUDED_PARTS = {'tests', 'benches', 'examples', 'evidence'}
TEST_PREDICATE = re.compile(r'(?<![\w"])test(?![\w"])')
NOT_PREDICATE = re.compile(r'^\s*not\s*\(')
VISIBILITY = re.compile(r'\s*(?:pub(?:\s*\([^)]*\))?\s+)?')
MOD_ITEM = re.compile(r'(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+\w+')
HINT = ('Keep test-only code inside #[cfg(test)] modules; production items must '
        'not fork on the test cfg')


def masked_source(text):
    """Comment-, string- and test-module-masked source with offsets preserved.

    Reuses the hardcode guard's lexer and production() masking: genuine
    #[cfg(test)] / #[cfg(all(test, ...))] `mod name { ... }` blocks and
    #[test] functions are blanked, everything else stays visible.
    """
    visible, literals = hg.production(text)
    chars = list(visible)
    for start, end, _ in literals:
        chars[start:end] = hg.blank(visible[start:end])
    return ''.join(chars)


def scan_balanced(text, open_paren):
    """Index just past the ')' matching text[open_paren] == '(', or -1."""
    depth = 0
    i = open_paren
    while i < len(text):
        c = text[i]
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return -1


def skip_attributes(text, i):
    """From index i, skip whitespace and any #[...] attributes."""
    n = len(text)
    while True:
        while i < n and text[i].isspace():
            i += 1
        if text.startswith('#', i):
            j = i + 1
            while j < n and text[j].isspace():
                j += 1
            if j < n and text[j] == '[':
                depth = 0
                k = j
                while k < n:
                    if text[k] == '[':
                        depth += 1
                    elif text[k] == ']':
                        depth -= 1
                        if depth == 0:
                            i = k + 1
                            break
                    k += 1
                else:
                    return n
                continue
        return i


def followed_by_mod(text, i):
    """True when only attributes/visibility sit between i and a `mod` item."""
    i = skip_attributes(text, i)
    match = VISIBILITY.match(text, i)
    if match:
        i = match.end()
    return MOD_ITEM.match(text, i) is not None


def cfg_inner(text, i):
    """If a #[cfg( at i, return (inner, end_index_after_]); else None."""
    m = re.match(r'#\s*\[\s*cfg\s*\(', text[i:])
    if not m:
        return None
    open_paren = i + m.end() - 1
    close = scan_balanced(text, open_paren)
    if close < 0:
        return None
    j = close
    while j < len(text) and text[j].isspace():
        j += 1
    if j >= len(text) or text[j] != ']':
        return None
    return text[open_paren + 1:close - 1], j + 1


def cfg_bang_inner(text, i):
    """If a cfg!( at i, return (inner, end_index); else None."""
    m = re.match(r'cfg\s*!\s*\(', text[i:])
    if not m:
        return None
    open_paren = i + m.end() - 1
    close = scan_balanced(text, open_paren)
    if close < 0:
        return None
    return text[open_paren + 1:close - 1], close


def iter_cfg_forks(masked):
    """Yield (offset, inner, kind) for every test-forking cfg in masked source.

    kind is 'cfg_attr' for #[cfg(...)] attributes and 'cfg_bang' for cfg!(...).
    Attributes that merely declare a test module (`mod` item, possibly behind
    other attributes like #[path]) are exempt; negated test cfgs are not.
    """
    i = 0
    n = len(masked)
    while i < n:
        if masked[i] == '#' or masked.startswith('cfg', i):
            found = cfg_inner(masked, i) if masked[i] == '#' else None
            if found is None and masked.startswith('cfg', i):
                bang = cfg_bang_inner(masked, i)
                if bang and TEST_PREDICATE.search(bang[0]):
                    yield i, bang[0], 'cfg_bang'
                    i = bang[1]
                    continue
            elif found is not None:
                inner, end = found
                if TEST_PREDICATE.search(inner):
                    negative = NOT_PREDICATE.match(inner) is not None
                    if negative or not followed_by_mod(masked, end):
                        yield i, inner, 'cfg_attr'
                i = end
                continue
        i += 1


def iter_findings(masked, original):
    for offset, inner, kind in iter_cfg_forks(masked):
        line = original.count('\n', 0, offset) + 1
        rule = ('cfg_test_negation_in_production' if NOT_PREDICATE.match(inner)
                else 'cfg_test_fork_in_production') if kind == 'cfg_attr' \
            else 'cfg_test_macro_in_production'
        yield dict(rule=rule, offset=offset, line=line, snippet=inner.strip()[:120])


def rust_files(root):
    """Every production .rs file: src/ plus each workspace crate's src/."""
    seen = set()
    candidates = [root / 'src']
    for manifest in sorted(root.glob('*/Cargo.toml')):
        srcdir = manifest.parent / 'src'
        if srcdir.is_dir():
            candidates.append(srcdir)
    for srcdir in candidates:
        for file in sorted(srcdir.rglob('*.rs')):
            rel = file.relative_to(root)
            if any(part in EXCLUDED_PARTS for part in rel.parts[:-1]):
                continue
            if rel.as_posix() not in seen:
                seen.add(rel.as_posix())
                yield file, rel.as_posix()


def audit(root, baseline, bootstrap=False):
    findings = []
    counts = {}
    improvements = []
    scanned = 0
    for file, path in rust_files(root):
        scanned += 1
        text = file.read_text(encoding='utf-8')
        masked = masked_source(text)
        hits = list(iter_findings(masked, text))
        counts[path] = len(hits)
        limit = baseline.get(path, 0)
        if len(hits) > limit and not bootstrap:
            for hit in hits[limit:]:
                findings.append(dict(rule=hit['rule'], file=path, line=hit['line'],
                                     literal=hit['snippet'], hint=HINT))
        elif len(hits) < limit:
            improvements.append(dict(file=path, current=len(hits), baseline=limit))
    for path, limit in baseline.items():
        if path not in counts and limit:
            improvements.append(dict(file=path, current=0, baseline=limit))
    findings.sort(key=lambda f: (f['file'], f['line'], f['rule']))
    return dict(ok=not findings, files_scanned=scanned, findings=findings,
                improvements=improvements,
                counts={p: n for p, n in sorted(counts.items()) if n})


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--update-baseline', action='store_true')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args(argv)
    root = args.root.resolve()
    baseline_path = root / BASELINE
    try:
        if not root.is_dir() or not (root / 'src').is_dir():
            raise ValueError('root must contain a src/ directory')
        if baseline_path.exists():
            data = json.loads(baseline_path.read_text(encoding='utf-8'))
            baseline = data['counts']
            if (not isinstance(baseline, dict) or any(not isinstance(k, str) or
                    type(v) is not int or v < 0 for k, v in baseline.items())):
                raise ValueError('baseline counts must be nonnegative integers by file')
        elif args.update_baseline:
            baseline = {}
        else:
            raise ValueError(f'missing baseline: {baseline_path}')
        report = audit(root, baseline, bootstrap=not baseline_path.exists())
        if args.update_baseline and report['ok']:
            # Existing ceilings may only be lowered, never raised.
            new = {p: min(n, baseline.get(p, n)) for p, n in report['counts'].items()}
            baseline_path.parent.mkdir(parents=True, exist_ok=True)
            baseline_path.write_text(json.dumps({'counts': new}, indent=2,
                                                sort_keys=True) + '\n', encoding='utf-8')
            report['improvements'] = []
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f'cfg fork audit: {error}', file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"cfg fork audit: {'PASS' if report['ok'] else 'FAIL'} "
              f"({report['files_scanned']} Rust files, {len(report['findings'])} findings)")
        for finding in report['findings']:
            print(f"  {finding['file']}:{finding['line']}: {finding['rule']}: {finding['literal']}\n"
                  f"    {finding['hint']}")
        for item in report['improvements']:
            print(f"  DOWN {item['file']}: {item['current']} < {item['baseline']}; tighten the baseline")
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
