#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Guard generic Rust against mod-ID literals and vanilla-only namespace traps.

Usage: python3 tools/hardcode_guard_audit.py [--root DIR] [--json] [--update-baseline]
Exit: 0 pass, 1 regression, 2 invalid input or unreadable baseline/source.
The baseline is a per-file ceiling; updates may lower ceilings but never raise them.
"""

import argparse
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
BASELINE = Path('audit/hardcode_guard_baseline.json')
NAMESPACES = ('create', 'farmersdelight', 'iceandfire', 'twilightforest', 'botania',
              'ars_nouveau', 'mekanism', 'appliedenergistics2', 'ae2',
              'biomesoplenty', 'thermal', 'aether', 'immersive_portals')
RAW = re.compile(r'(?:b)?r(#+)?"')
CHAR = re.compile(r"'(?:\\.|[^'\\\n])'")
MOD_ID = re.compile(r'(?<![\w])(?:' + '|'.join(NAMESPACES) + r'):', re.I)
SUFFIX = re.compile(r'\.starts_with\s*\(\s*"minecraft:"\s*\).*?\.ends_with\s*\(', re.S)
FORMAT = r'format!\s*\(\s*"minecraft:\{[^"\n]*"\s*(?:,\s*[^()]*)?\)'
FORCED = re.compile(r'(?:' + FORMAT + r'\s*(?:==|!=)|(?:==|!=)\s*' + FORMAT
                    + r'|\.contains\s*\(\s*' + FORMAT + r')')
HINT = 'Use registry facts, #tags, or texture/property derivation instead of hardcoding mod IDs'


def blank(text):
    return ''.join('\n' if c == '\n' else ' ' for c in text)


def lex(text):
    """Return comment-masked source and literal spans, preserving offsets/newlines.

    Supports nested block comments, escaped strings, raw strings and byte strings.
    Character literals are skipped without mistaking Rust lifetimes for quotes.
    """
    chars = list(text)
    literals = []
    i = 0
    while i < len(text):
        start = i
        if text.startswith('//', i):
            end = text.find('\n', i)
            i = len(text) if end < 0 else end
            chars[start:i] = blank(text[start:i])
            continue
        if text.startswith('/*', i):
            depth = 1
            i += 2
            while i < len(text) and depth:
                if text.startswith('/*', i):
                    depth += 1
                    i += 2
                elif text.startswith('*/', i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            chars[start:i] = blank(text[start:i])
            continue
        raw = RAW.match(text, i) if (i == 0 or not (text[i-1].isalnum() or text[i-1] == '_')) else None
        if raw:
            hashes = raw.group(1) or ''
            i += len(raw.group())
            close = text.find('"' + hashes, i)
            i = len(text) if close < 0 else close + 1 + len(hashes)
            literals.append((start, i, text[start:i]))
            continue
        prefix = 1 if text.startswith('b"', i) and (i == 0 or not text[i-1].isalnum()) else 0
        if text[i] == '"' or prefix:
            i += 1 + prefix
            while i < len(text):
                if text[i] == '\\':
                    i += 2
                elif text[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            literals.append((start, i, text[start:i]))
            continue
        if text[i] == "'" and (i + 2 < len(text)):
            match = CHAR.match(text, i)
            if match:
                i += len(match.group())
                chars[start:i] = blank(text[start:i])
                continue
        i += 1
    return ''.join(chars), literals


def production(text):
    """Mask cfg(test) modules, including their attributes and nested braces."""
    source, literals = lex(text)
    syntax = list(source)
    for start, end, _ in literals:
        syntax[start:end] = blank(source[start:end])
    syntax = ''.join(syntax)
    removed = []
    pattern = re.compile(r'#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{')
    for match in pattern.finditer(syntax):
        if any(a <= match.start() < b for a, b in removed):
            continue
        depth = 1
        end = match.end()
        while end < len(syntax) and depth:
            if syntax[end] == '{':
                depth += 1
            elif syntax[end] == '}':
                depth -= 1
            end += 1
        removed.append((match.start(), end))
    visible = list(source)
    for start, end in removed:
        visible[start:end] = blank(source[start:end])
    return ''.join(visible), [(a, b, s) for a, b, s in literals
                             if not any(start <= a < end for start, end in removed)]


def paths(root, directories):
    for directory in directories:
        yield from sorted((root / directory).glob('*.rs'))


def generic(path):
    return (path in ('client-port/src/dynamic_light_resolver.rs',
                     'client-port/src/builtin_shadow_wgsl.rs',
                     'server-files/src/directional_shadow.rs',
                     'server-files/src/mods_dir.rs',
                     'mod-adapter-core/src/colossal_and_forge.rs',
                     'mod-adapter-core/src/capability_bus.rs',
                     'mod-adapter-core/src/unified_tags.rs',
                     'proto/src/seamless_portal.rs') or path.startswith('mod-offload/src/'))


def audit(root, baseline, trap_baseline=None, bootstrap=False):
    trap_baseline = trap_baseline or {}
    dirs = ('src', 'client-port/src', 'client-gpu/src', 'client-app/src',
            'mod-adapter-core/src', 'mod-offload/src', 'server-files/src', 'proto/src')
    spread_dirs = ('src/', 'client-port/src/', 'client-gpu/src/', 'client-app/src/')
    excluded = ('src/live_mod_bridge.rs', 'src/mod_offload_bridge.rs')
    findings = []
    counts = {}
    trap_counts = {}
    improvements = []
    scanned = 0
    for file in paths(root, dirs):
        path = file.relative_to(root).as_posix()
        # Other server/proto/adapter files are outside the scope of these rules.
        if not (generic(path) or path.startswith(('src/', 'client-port/src/',
                       'client-gpu/src/', 'client-app/src/', 'mod-adapter-core/src/'))):
            continue
        scanned += 1
        text = file.read_text(encoding='utf-8')
        visible, literals = production(text)
        occurrences = []
        for offset, _, literal in literals:
            for match in MOD_ID.finditer(literal):
                line = text.count('\n', 0, offset + match.start()) + 1
                occurrences.append((line, literal))
                if generic(path):
                    findings.append(dict(rule='zero_mod_id_in_generic_engine', file=path,
                                         line=line, literal=literal, hint=HINT))
        if path.startswith(spread_dirs) and path not in excluded and not path.endswith('_tests.rs'):
            counts[path] = len(occurrences)
            limit = baseline.get(path, 0)
            if len(occurrences) > limit and not bootstrap:
                for line, literal in occurrences[limit:]:
                    findings.append(dict(rule='mod_namespace_spread_ratchet', file=path,
                                         line=line, literal=literal, hint=HINT))
            elif len(occurrences) < limit:
                improvements.append(dict(file=path, current=len(occurrences), baseline=limit))
        if path.startswith(('src/', 'client-port/src/', 'client-gpu/src/',
                            'client-app/src/', 'mod-adapter-core/src/')):
            traps = []
            if not path.endswith('_tests.rs'):
                for line_no, line in enumerate(visible.splitlines(), 1):
                    # Within one expression/line; comments and tests already blanked.
                    if SUFFIX.search(line) or FORCED.search(line):
                        traps.append((line_no, line.strip()))
            trap_counts[path] = len(traps)
            limit = trap_baseline.get(path, 0)
            if len(traps) > limit and not bootstrap:
                for line_no, literal in traps[limit:]:
                    findings.append(dict(rule='vanilla_only_namespace_suffix_trap',
                                         file=path, line=line_no, literal=literal,
                                         hint='Compare namespaced registry facts or #tags, not forced minecraft: names'))
            elif len(traps) < limit:
                improvements.append(dict(file=path, current=len(traps), baseline=limit,
                                         rule='vanilla_only_namespace_suffix_trap'))
    for path, limit in trap_baseline.items():
        if path not in trap_counts and limit:
            improvements.append(dict(file=path, current=0, baseline=limit,
                                     rule='vanilla_only_namespace_suffix_trap'))
    for path, limit in baseline.items():
        if path not in counts and limit:
            improvements.append(dict(file=path, current=0, baseline=limit))
    findings.sort(key=lambda f: (f['file'], f['line'], f['rule']))
    return dict(ok=not findings, files_scanned=scanned, findings=findings,
                improvements=improvements, counts={p: n for p, n in sorted(counts.items()) if n},
                namespace_traps={p: n for p, n in sorted(trap_counts.items()) if n})


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
            traps = data['namespace_traps']
            if any(not isinstance(table, dict) or any(not isinstance(k, str) or
                    type(v) is not int or v < 0 for k, v in table.items())
                   for table in (baseline, traps)):
                raise ValueError('baseline counts must be nonnegative integers by file')
        elif args.update_baseline:
            baseline = {}
            traps = {}
        else:
            raise ValueError(f'missing baseline: {baseline_path}')
        report = audit(root, baseline, traps, bootstrap=not baseline_path.exists())
        if args.update_baseline and report['ok']:
            # Existing ceilings may only be lowered, never raised.
            new = {p: min(n, baseline.get(p, n)) for p, n in report['counts'].items()}
            baseline_path.parent.mkdir(parents=True, exist_ok=True)
            new_traps = {p: min(n, traps.get(p, n)) for p, n in report['namespace_traps'].items()}
            baseline_path.write_text(json.dumps({'counts': new, 'namespace_traps': new_traps}, indent=2, sort_keys=True) + '\n', encoding='utf-8')
            report['improvements'] = []
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f'hardcode guard: {error}', file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"hardcode guard: {'PASS' if report['ok'] else 'FAIL'} ({report['files_scanned']} Rust files, {len(report['findings'])} findings)")
        for finding in report['findings']:
            print(f"  {finding['file']}:{finding['line']}: {finding['rule']}: {finding['literal']}\n    {finding['hint']}")
        for item in report['improvements']:
            print(f"  DOWN {item['file']}: {item['current']} < {item['baseline']}; tighten the baseline")
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
