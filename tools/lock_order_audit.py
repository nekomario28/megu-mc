#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Guard production Rust against global-lock ordering hazards.

Scans src/**/*.rs for functions that acquire more than one process-global
Mutex/RwLock (direct &'static accessors, OnceLock/LazyLock statics, thin
&Mutex/&RwLock guard helpers, and zero-arg guard-returning proxies such as
region_guard()). For every function body outside #[cfg(test)] modules it
extracts the ordered sequence of distinct global locks that are held at the
same time — brace/let-binding/drop scope aware, deliberately lightweight and
deterministic — and fails on:

* ABBA lock-order inversions: one function acquires X then Y while another
  (or the same function on another path) acquires Y then X, and
* functions holding three or more global locks simultaneously.

Usage: python3 tools/lock_order_audit.py [--root DIR] [--json] [--update-baseline]
Exit: 0 pass, 1 regression, 2 invalid input or unreadable baseline/source.
The baseline is a per-file ceiling; updates may lower ceilings but never raise them.
"""

import argparse
import json
from pathlib import Path
import re
import sys
from bisect import bisect_right

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hardcode_guard_audit as hg
import cfg_fork_audit as cf

ROOT = Path(__file__).resolve().parents[1]
BASELINE = Path('audit/lock_order_baseline.json')
MAX_PROXY_PASSES = 6

HINT = ('Acquire global locks in one consistent order, or drop each guard before '
        'taking the next; holding three or more global locks at once is forbidden')

TOKEN_RE = re.compile(r"""
    (?P<ident>[A-Za-z_]\w*)
  | (?P<num>\d[\d_]*(?:\.\d+)?)
  | (?P<op>::|->|=>|==|!=|>=|<=|&&|\|\||<<|>>|\.\.|\+=|-=|\*=|/=|%=|&=|\|=|\^=|<<=|>>=)
  | (?P<punct>[-+*/%<>=!&|?:;,.@\[\]{}()'#~^])
""", re.X)

LOCK_METHODS = ('lock', 'read', 'write')
# Method calls that return (or wrap) the same guard, so a let binding still owns it.
TRANSPARENT = {'unwrap', 'unwrap_or_else', 'expect', 'map_err', 'ok',
               'as_ref', 'as_mut', 'deref', 'borrow'}
PATTERN_KEYWORDS = {
    'mut', 'ref', 'box', 'self', 'Self', 'crate', 'super', 'Some', 'None',
    'Ok', 'Err', 'move', 'async', 'const', 'static', 'dyn', 'impl', 'where',
    'if', 'else', 'match', 'for', 'in', 'while', 'loop', 'return', 'break',
    'continue', 'true', 'false', 'as', 'is', 'let', 'fn', 'struct', 'enum',
    'union', 'mod', 'use', 'pub', 'trait', 'type', 'become', 'abstract',
    'final', 'override', 'priv', 'typeof', 'unsized', 'yield', 'do', 'macro',
    'try', 'await', 'gen',
}
FN_RE = re.compile(r'\bfn\s+([A-Za-z_]\w*)\s*(<[^{;]*>)?\s*\(')
STATIC_RE = re.compile(r'\bstatic\s+(?:mut\s+)?([A-Za-z_]\w*)\s*:\s*([^;]+?)=')
ALIAS_RE = re.compile(r'\btype\s+([A-Za-z_]\w*)\s*(?:<[^{;]*>)?\s*=\s*([^;]+);', re.S)
ACCESSOR_RET_RE = re.compile(r"&\s*'static\s+")
WRAPPER_PARAM_RE = re.compile(
    r'([A-Za-z_]\w*)\s*:\s*&\s*(?:\'\w+\s+)?(?:mut\s+)?'
    r'(?:(?:[A-Za-z_]\w*)::)*(OnceLock|LazyLock|Mutex|RwLock|[A-Za-z_]\w*)\s*<')


def mask_test_modules(text):
    """Blank comments, literals and genuine #[cfg(test)] `mod name { ... }` blocks.

    Unlike the cfg-fork audit, compound cfgs such as
    #[cfg(all(test, feature))] stay visible: their modules are feature-gated
    test fixtures whose lock usage is exactly what this gate must see.
    """
    masked, literals = hg.lex(text)
    chars = list(masked)
    for start, end, _ in literals:
        chars[start:end] = hg.blank(masked[start:end])
    code = ''.join(chars)
    removed = []
    for match in re.finditer(r'#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]', code):
        i = cf.skip_attributes(code, match.end())
        vis = cf.VISIBILITY.match(code, i)
        if vis:
            i = vis.end()
        mod = cf.MOD_ITEM.match(code, i)
        if not mod:
            continue
        j = mod.end()
        while j < len(code) and code[j].isspace():
            j += 1
        if j >= len(code) or code[j] != '{':
            continue  # `mod name;` declaration: the file is scanned on its own
        depth = 1
        end = j + 1
        while end < len(code) and depth:
            if code[end] == '{':
                depth += 1
            elif code[end] == '}':
                depth -= 1
            end += 1
        removed.append((match.start(), end))
    visible = list(code)
    for start, end in removed:
        visible[start:end] = hg.blank(code[start:end])
    return ''.join(visible)


def tokenize(text):
    """Tokens as (kind, text, char_pos); positions list built separately."""
    tokens = []
    for match in TOKEN_RE.finditer(text):
        tokens.append((match.lastgroup, match.group(), match.start()))
    return tokens


def matching_delim(text, open_pos, open_c, close_c):
    depth = 0
    i = open_pos
    while i < len(text):
        if text[i] == open_c:
            depth += 1
        elif text[i] == close_c:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def find_fns(code):
    """All fn items with body spans; declarations without bodies are skipped."""
    fns = []
    for match in FN_RE.finditer(code):
        name = match.group(1)
        open_paren = code.index('(', match.end() - 1)
        close_paren = matching_delim(code, open_paren, '(', ')')
        if close_paren < 0:
            continue
        params = code[open_paren + 1:close_paren]
        i = close_paren + 1
        brackets = 0
        found = False
        while i < len(code):
            c = code[i]
            if c == '[':
                brackets += 1
            elif c == ']':
                brackets -= 1
            elif brackets == 0:
                if c == '{':
                    found = True
                    break
                if c == ';':
                    break
            i += 1
        if not found:
            continue
        depth = 1
        end = i + 1
        while end < len(code) and depth:
            if code[end] == '{':
                depth += 1
            elif code[end] == '}':
                depth -= 1
            end += 1
        ret = code[close_paren + 1:i]
        fns.append(dict(name=name, params=params, ret=ret,
                        body_open=i, body_close=end - 1))
    return fns


def strip_sync_path(type_expr):
    return re.sub(r'^(?:(?:std|core)::sync::)+', '', type_expr.strip())


def type_head(type_expr):
    match = re.match(r'(?:[A-Za-z_]\w*::)*([A-Za-z_]\w*)', strip_sync_path(type_expr))
    return match.group(1) if match else None


def resolve_lock_kind(type_expr, aliases, depth=0):
    """'mutex' | 'rwlock' | None for a type expression."""
    if depth > 6 or not type_expr:
        return None
    expr = strip_sync_path(type_expr)
    match = re.match(r'(OnceLock|LazyLock)\s*<', expr)
    if match:
        open_pos = expr.index('<', match.end() - 1)
        close = matching_delim(expr, open_pos, '<', '>')
        if close < 0:
            return None
        return resolve_lock_kind(expr[open_pos + 1:close], aliases, depth + 1)
    head = type_head(expr)
    if head == 'Mutex':
        return 'mutex'
    if head == 'RwLock':
        return 'rwlock'
    if head in aliases:
        return resolve_lock_kind(aliases[head], aliases, depth + 1)
    return None


def collect_aliases(code):
    """type NAME = <lock-backed rhs>; resolved recursively."""
    aliases = {}
    for _ in range(4):
        for match in ALIAS_RE.finditer(code):
            name, rhs = match.group(1), match.group(2)
            if name not in aliases and resolve_lock_kind(rhs, aliases):
                aliases[name] = rhs
    return aliases


def collect_statics(code, aliases):
    """static NAME: <lock type> -> canonical lock id (the static name)."""
    locks = {}
    for match in STATIC_RE.finditer(code):
        name, type_expr = match.group(1), match.group(2)
        if resolve_lock_kind(type_expr, aliases):
            locks[name] = name
    return locks


def collect_accessors(fns, aliases):
    """fn name() -> &'static <lock type>; value is the fn name (pre-alias)."""
    accessors = {}
    for fn in fns:
        match = ACCESSOR_RET_RE.search(fn['ret'])
        if match and resolve_lock_kind(fn['ret'][match.end():], aliases):
            accessors[fn['name']] = fn['name']
    return accessors


def collect_wrappers(fns, aliases):
    """Free fns taking a &lock-type param and returning a guard.

    Returns name -> (param_name, method) where method is the lock/read/write
    call the body makes on that param.
    """
    wrappers = {}
    for fn in fns:
        if re.search(r'\bself\b', fn['params']):
            continue
        if 'Guard' not in fn['ret']:
            continue
        for match in WRAPPER_PARAM_RE.finditer(fn['params']):
            pname, head = match.group(1), match.group(2)
            kind = ('mutex' if head == 'Mutex' else 'rwlock' if head == 'RwLock'
                    else resolve_lock_kind(head + '<>', aliases))
            if head in ('OnceLock', 'LazyLock'):
                kind = 'maybe'
            if not kind:
                continue
            body = code_slice = fn.get('body_text', '')
            # method: first .lock()/.read()/.write() applied to the param,
            # allowing a short get_or_init-style chain in between
            m = re.search(re.escape(pname) +
                          r'(?:\s*\.\s*\w+\s*\([^;\n]*?\))*\s*\.\s*'
                          r'(lock|read|write)\s*\(', body, re.S)
            if m:
                wrappers[fn['name']] = (pname, m.group(1))
                break
    return wrappers


def skip_parens(tokens, open_idx, hi):
    depth = 0
    j = open_idx
    while j < hi:
        if tokens[j][1] == '(':
            depth += 1
        elif tokens[j][1] == ')':
            depth -= 1
            if depth == 0:
                return j
        j += 1
    return None


def lock_expr_arg(tokens, i, hi):
    """Parse `& accessor()` / `accessor()` / `& STATIC` / `STATIC` at token i.

    Returns (kind, name, end_token_index) or None. kind is 'call' for
    accessor() and 'name' for a bare static.
    """
    j = i
    while j < hi and tokens[j][1] in ('&', 'mut'):
        j += 1
    if j + 1 < hi and tokens[j][0] == 'punct' and tokens[j][1] == "'" \
            and tokens[j + 1][0] == 'ident':
        j += 2  # lifetime
    if j >= hi or tokens[j][0] != 'ident':
        return None
    name = tokens[j][1]
    if j + 2 < hi and tokens[j + 1][1] == '(' and tokens[j + 2][1] == ')':
        return 'call', name, j + 3
    return 'name', name, j + 1


def match_static_chain(tokens, i, hi):
    """STATIC .seg().seg()? .lock() — first lock-method ends the chain."""
    j = i + 1
    while j < hi:
        if tokens[j][1] == '?':
            j += 1
            continue
        if tokens[j][1] != '.' or j + 1 >= hi or tokens[j + 1][0] != 'ident':
            return None
        method = tokens[j + 1][1]
        k = j + 2
        if k < hi and tokens[k][1] == '(':
            close = skip_parens(tokens, k, hi)
            if close is None:
                return None
            if method in LOCK_METHODS and tokens[k + 1][1] == ')':
                return method, close + 1
            j = close + 1
            continue
        return None  # bare field access: not a lock chain
    return None


def match_acquisition(tokens, i, hi, providers):
    """At ident token i, try accessor/static/wrapper/proxy patterns."""
    name = tokens[i][1]
    # accessor(): name ( ) . method ( )
    if name in providers['accessors'] and i + 6 < hi \
            and tokens[i + 1][1] == '(' and tokens[i + 2][1] == ')' \
            and tokens[i + 3][1] == '.' \
            and tokens[i + 4][0] == 'ident' and tokens[i + 4][1] in LOCK_METHODS \
            and tokens[i + 5][1] == '(' and tokens[i + 6][1] == ')':
        return dict(locks=[providers['accessors'][name]],
                    method=tokens[i + 4][1], end=i + 7, tpos=i)
    # wrapper(accessor()|&STATIC)
    if name in providers['wrappers'] and i + 1 < hi and tokens[i + 1][1] == '(':
        arg = lock_expr_arg(tokens, i + 2, hi)
        if arg is not None:
            kind_, argname, arg_end = arg
            if arg_end < hi and tokens[arg_end][1] == ')':
                if kind_ == 'call' and argname in providers['accessors']:
                    lock = providers['accessors'][argname]
                elif kind_ == 'name' and argname in providers['statics']:
                    lock = providers['statics'][argname]
                else:
                    lock = None
                if lock:
                    return dict(locks=[lock],
                                method=providers['wrappers'][name][1],
                                end=arg_end + 1, tpos=i)
    # proxy(): name ( ) — zero-arg guard-returning fn
    if name in providers['proxies'] and i + 2 < hi \
            and tokens[i + 1][1] == '(' and tokens[i + 2][1] == ')':
        locks = sorted(providers['proxy_escapes'].get(name, ()))
        if locks:
            return dict(locks=locks, method='lock', end=i + 3, tpos=i)
    # STATIC . chain . method ( )
    if name in providers['statics']:
        res = match_static_chain(tokens, i, hi)
        if res is not None:
            method, end = res
            return dict(locks=[providers['statics'][name]], method=method,
                        end=end, tpos=i)
    return None


def drop_arg(tokens, i, hi):
    if i + 3 < hi and tokens[i + 1][1] == '(' \
            and tokens[i + 2][0] == 'ident' and tokens[i + 3][1] == ')':
        return tokens[i + 2][1]
    return None


def transparent_tail(tokens, j, hi):
    """Advance j past guard-transparent adapters; return index of stopper."""
    while j < hi:
        if tokens[j][1] == '?':
            j += 1
            continue
        if tokens[j][1] == '.' and j + 2 < hi \
                and tokens[j + 1][0] == 'ident' \
                and tokens[j + 1][1] in TRANSPARENT \
                and tokens[j + 2][1] == '(':
            close = skip_parens(tokens, j + 2, hi)
            if close is None:
                return j
            j = close + 1
            continue
        return j
    return j


def record_acquisition(st, acq, tokens):
    """Apply one acquisition (possibly a proxy batch) to held/edges/peak."""
    hi = st['hi']
    held_locks = {e['lock'] for e in st['held']}
    for lock in acq['locks']:
        for other in held_locks:
            if other != lock:
                st['edges'].append((other, lock, tokens[acq['tpos']][2]))
    simultaneous = held_locks | set(acq['locks'])
    if len(simultaneous) > st['peak']:
        st['peak'] = len(simultaneous)
        st['peak_locks'] = sorted(simultaneous)
    # bound vs temporary: a transparent tail ending at ';' keeps the let binding
    bound = False
    let = st['let']
    if let is not None and let['mode'] == 'init' and let.get('base'):
        bp, bb, bg = let['base']
        j = transparent_tail(tokens, acq['end'], hi)
        if j < hi and tokens[j][1] == ';' \
                and st['paren'] == bp and st['brack'] == bb and st['brace'] == bg:
            bound = True
    var = let['vars'][-1] if bound and let['vars'] else None
    for lock in acq['locks']:
        entry = {'lock': lock, 'var': var, 'scope': st['brace'],
                 'temp': not bound, 'tpos': acq['tpos']}
        st['held'].append(entry)
        st['pending'].append(entry)
        st['acquisitions'].append((lock, entry))


def let_token(st, tokens, i):
    """Advance the let-pattern/init state machine for token i."""
    let = st['let']
    kind, text, _ = tokens[i]
    if let['mode'] == 'pattern':
        if kind == 'punct' and text == '=':
            let['mode'] = 'init'
            let['base'] = (st['paren'], st['brack'], st['brace'])
        elif kind == 'ident' and text not in PATTERN_KEYWORDS and text != '_':
            let['vars'].append(text)
        elif kind == 'punct' and text == ';':
            st['let'] = None


def new_state(hi):
    return {'held': [], 'pending': [], 'let': None, 'paren': 0, 'brack': 0,
            'brace': 0, 'prev': None, 'hi': hi, 'edges': [], 'peak': 0,
            'peak_locks': [], 'acquisitions': []}


def scan_fn_segment(tokens, lo, hi, providers, st):
    st['hi'] = hi
    i = lo
    while i < hi:
        kind, text, pos = tokens[i]
        if text == '(':
            st['paren'] += 1
        elif text == ')':
            st['paren'] -= 1
        elif text == '[':
            st['brack'] += 1
        elif text == ']':
            st['brack'] -= 1
        elif text == '{':
            st['brace'] += 1
            for entry in st['pending']:
                entry['scope'] = st['brace']
                entry['temp'] = False
            st['pending'] = []
        elif text == '}':
            st['brace'] -= 1
            st['held'] = [e for e in st['held'] if e['scope'] <= st['brace']]
            st['pending'] = []
        elif text == ';':
            if st['paren'] == 0 and st['brack'] == 0:
                st['held'] = [e for e in st['held'] if not e['temp']]
            st['pending'] = []
            st['let'] = None
        elif kind == 'ident' and text == 'let' and st['prev'] != '.':
            st['let'] = {'vars': [], 'mode': 'pattern'}
        elif st['let'] is not None:
            let_token(st, tokens, i)
        if kind == 'ident' and st['prev'] != '.':
            acq = match_acquisition(tokens, i, hi, providers)
            if acq is not None:
                record_acquisition(st, acq, tokens)
                i = acq['end']
        if kind == 'ident' and text == 'drop' and st['prev'] != '.':
            var = drop_arg(tokens, i, hi)
            if var:
                st['held'] = [e for e in st['held'] if e['var'] != var]
        st['prev'] = text
        i += 1


def fn_segments(tokens, positions, fns):
    """Token-index ranges of each fn body excluding nested fn bodies."""
    spans = []
    for fn in fns:
        lo = bisect_right(positions, fn['body_open'] - 1)
        hi = bisect_right(positions, fn['body_close'])
        spans.append((lo, hi))
    segments = {}
    for idx, fn in enumerate(fns):
        lo, hi = spans[idx]
        cuts = []
        for j, other in enumerate(fns):
            if j == idx:
                continue
            if fn['body_open'] < other['body_open'] < fn['body_close'] \
                    and other['body_close'] <= fn['body_close']:
                olo = bisect_right(positions, other['body_open'] - 1)
                ohi = bisect_right(positions, other['body_close'])
                cuts.append((olo, ohi))
        cuts.sort()
        segs = []
        cur = lo
        for clo, chi in cuts:
            if clo > cur:
                segs.append((cur, clo))
            cur = max(cur, chi)
        if cur < hi:
            segs.append((cur, hi))
        segments[idx] = segs
    return segments


def exit_regions(tokens, segments):
    """Token indices covering tail expressions and return statements."""
    covered = set()
    for lo, hi in segments:
        last_semi = lo - 1
        for i in range(lo, hi):
            if tokens[i][0] == 'ident' and tokens[i][1] == 'return':
                j = i + 1
                while j < hi and tokens[j][1] != ';':
                    covered.add(j)
                    j += 1
            elif tokens[i][1] == ';':
                last_semi = i
        for i in range(last_semi + 1, hi):
            covered.add(i)
    return covered


def compute_escapes(st, tokens, exit_set):
    """Locks whose guard may outlive the function (returned / tail expr)."""
    escapes = set()
    exit_vars = {tokens[i][1] for i in exit_set if tokens[i][0] == 'ident'}
    for entry in st['held']:
        if entry['var'] and entry['var'] in exit_vars:
            escapes.add(entry['lock'])
    for lock, entry in st['acquisitions']:
        if entry['tpos'] in exit_set or (entry['var'] and entry['var'] in exit_vars):
            escapes.add(lock)
    return escapes


def prepare_file(path, text):
    """Mask, tokenize and segment one file once per audit run."""
    code = mask_test_modules(text)
    tokens = tokenize(code)
    positions = [t[2] for t in tokens]
    fns = find_fns(code)
    for fn in fns:
        fn['body_text'] = code[fn['body_open']:fn['body_close']]
    segments = fn_segments(tokens, positions, fns) if fns else {}
    return dict(path=path, text=text, code=code, tokens=tokens, fns=fns,
                segments=segments)


def scan_prepared(prep, providers):
    """Scan every fn body of a prepared file; returns per-fn result dicts."""
    tokens = prep['tokens']
    results = []
    for idx, fn in enumerate(prep['fns']):
        st = new_state(0)
        for lo, hi in prep['segments'][idx]:
            scan_fn_segment(tokens, lo, hi, providers, st)
        exit_set = exit_regions(tokens, prep['segments'][idx])
        results.append(dict(path=prep['path'], fn=fn, edges=st['edges'],
                            peak=st['peak'], peak_locks=st['peak_locks'],
                            escapes=compute_escapes(st, tokens, exit_set)))
    return results


def analyze_file(path, text, providers):
    """Convenience wrapper: prepare + scan a single file."""
    return scan_prepared(prepare_file(path, text), providers)


def line_of(text, pos):
    return text.count('\n', 0, pos) + 1


def parse_tree(root):
    """Read all src/**/*.rs once; collect providers and per-file texts."""
    srcdir = root / 'src'
    items = []
    providers = {'accessors': {}, 'statics': {}, 'wrappers': {}, 'proxies': {},
                 'proxy_escapes': {}}
    for file in sorted(srcdir.rglob('*.rs')):
        path = file.relative_to(root).as_posix()
        text = file.read_text(encoding='utf-8')
        prep = prepare_file(path, text)
        code = prep['code']
        aliases = collect_aliases(code)
        fns = prep['fns']
        statics = collect_statics(code, aliases)
        accessors = collect_accessors(fns, aliases)
        # canonical lock id: an accessor backed by a visible static aliases to it
        for fn in fns:
            if fn['name'] in accessors:
                for sname in statics:
                    if re.search(r'\b' + re.escape(sname) + r'\b', fn['body_text']):
                        accessors[fn['name']] = sname
                        break
        wrappers = collect_wrappers(fns, aliases)
        items.append(prep)
        providers['statics'].update(statics)
        providers['accessors'].update(accessors)
        for name, val in wrappers.items():
            providers['wrappers'].setdefault(name, val)
        for fn in fns:
            if 'Guard' in fn['ret'] and not re.search(r'\bself\b', fn['params']):
                providers['proxies'].setdefault(fn['name'], fn)
    return items, providers


def audit(root, baseline, bootstrap=False):
    items, providers = parse_tree(root)
    # proxy fixpoint: escapes feed proxy() acquisitions feed escapes
    results = []
    prev = None
    for _ in range(MAX_PROXY_PASSES):
        results = []
        for item in items:
            results.extend(scan_prepared(item, providers))
        escapes = {}
        for res in results:
            escapes[res['fn']['name']] = res['escapes']
        if escapes == prev:
            break
        prev = escapes
        providers['proxy_escapes'] = escapes
    # findings
    edge_map = {}
    multi = []
    for res in results:
        fid = (res['path'], res['fn']['name'], res['fn']['body_open'])
        for held, new, _pos in res['edges']:
            edge_map.setdefault((held, new), set()).add(fid)
        if res['peak'] >= 3:
            multi.append((fid, res))
    texts = {item['path']: item['text'] for item in items}
    findings = []
    seen_pairs = set()
    for (a, b), fns_ab in sorted(edge_map.items()):
        reverse = edge_map.get((b, a))
        if not reverse:
            continue
        for fid_f in sorted(fns_ab):
            for fid_g in sorted(reverse):
                fn_pair = tuple(sorted({fid_f, fid_g}))
                pair_key = (tuple(sorted((a, b))), fn_pair)
                if pair_key in seen_pairs:
                    continue
                seen_pairs.add(pair_key)
                # one finding entry per file touched by the inversion
                seen_files = set()
                for fid, other in ((fid_f, fid_g), (fid_g, fid_f)):
                    if fid[0] in seen_files:
                        continue
                    seen_files.add(fid[0])
                    findings.append(dict(
                        rule='lock_order_inversion', file=fid[0], fn=fid[1],
                        line=line_of(texts[fid[0]], fid[2]),
                        other_file=other[0], other_fn=other[1],
                        locks=sorted((a, b)), hint=HINT))
    for fid, res in sorted(multi):
        findings.append(dict(rule='multi_global_lock', file=fid[0], fn=fid[1],
                             line=line_of(texts[fid[0]], fid[2]),
                             locks=res['peak_locks'], hint=HINT))
    # per-file finding lists -> ceiling check
    by_file = {}
    for finding in findings:
        by_file.setdefault(finding['file'], []).append(finding)
    report_findings = []
    counts = {}
    improvements = []
    for path, flist in by_file.items():
        flist.sort(key=lambda f: (f['line'], f['rule'], f.get('other_file', ''), f.get('other_fn', '')))
        counts[path] = len(flist)
        limit = baseline.get(path, 0)
        if len(flist) > limit and not bootstrap:
            report_findings.extend(flist[limit:])
        elif len(flist) < limit:
            improvements.append(dict(file=path, current=len(flist), baseline=limit))
    for path, limit in baseline.items():
        if path not in by_file and limit:
            improvements.append(dict(file=path, current=0, baseline=limit))
    report_findings.sort(key=lambda f: (f['file'], f['line'], f['rule']))
    return dict(ok=not report_findings,
                files_scanned=sum(1 for _ in (root / 'src').rglob('*.rs')),
                findings=report_findings, improvements=improvements,
                counts={p: n for p, n in sorted(counts.items()) if n},
                providers=dict(accessors=len(providers['accessors']),
                               statics=len(providers['statics']),
                               wrappers=len(providers['wrappers']),
                               proxies=len(providers['proxies'])))


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
            new = {p: min(n, baseline.get(p, n)) for p, n in report['counts'].items()}
            baseline_path.parent.mkdir(parents=True, exist_ok=True)
            baseline_path.write_text(json.dumps({'counts': new}, indent=2,
                                                sort_keys=True) + '\n', encoding='utf-8')
            report['improvements'] = []
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f'lock order audit: {error}', file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"lock order audit: {'PASS' if report['ok'] else 'FAIL'} "
              f"({report['files_scanned']} Rust files, {len(report['findings'])} findings)")
        for finding in report['findings']:
            extra = (f" vs {finding['other_file']}:{finding['other_fn']}"
                     if finding['rule'] == 'lock_order_inversion' else '')
            print(f"  {finding['file']}:{finding['line']}: {finding['rule']}{extra}: "
                  f"{','.join(finding['locks'])}\n    {finding['hint']}")
        for item in report['improvements']:
            print(f"  DOWN {item['file']}: {item['current']} < {item['baseline']}; tighten the baseline")
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
